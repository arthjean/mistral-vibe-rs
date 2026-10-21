//! The sessions the Unified Harness serves for this service: each call is a
//! request on the session's own connection, and each turn runs on its own,
//! reference `AppServerSession` (`vibe/app_server/session.py`).

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};
use vibe_core::engine::TurnOutcome;

use super::{ClientFuture, SessionBackend, TurnUpdates};
use crate::client::unified::UnifiedLink;
use crate::client::{
    AppServer, ClientError, EventObserver, InterruptOutcome, PendingPublicCall, PreparedImages,
    ProgrammaticTurn, PublicContentBlock, PublicError, ScheduledTurn, SessionView, TurnRequest,
    TurnReservation,
};

pub(super) struct UnifiedSessions<'a> {
    pub(super) link: &'a mut UnifiedLink,
    pub(super) server: &'a AppServer,
}

/// An observer for a turn nothing engine-side reports on: a unified session
/// publishes its turn itself.
struct Unobserved;

impl EventObserver for Unobserved {
    fn observe(&self, _event: &vibe_core::events::EventEnvelope) -> Result<(), String> {
        Ok(())
    }
}

/// A turn the session runs and publishes itself, read through the channel
/// the link parked for it.
pub(super) fn turn_updates(link: &UnifiedLink, turn_id: &str) -> Result<TurnUpdates, ClientError> {
    Ok((Arc::new(Unobserved), link.updates(turn_id)?))
}

impl UnifiedSessions<'_> {
    /// Dispatches `method` on the connection of `session_id`, its answer
    /// required now.
    pub(super) fn call(
        &mut self,
        session_id: &str,
        method: &str,
        params: Value,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        self.link
            .dispatch(self.server, session_id, method, params)?
            .now()
    }

    /// Dispatches `method` on the connection of `session_id`, its answer and
    /// whatever it deferred left for the caller to complete.
    pub(super) fn begin_call(
        &mut self,
        session_id: &str,
        method: &str,
        params: Value,
    ) -> Result<PendingPublicCall, ClientError> {
        let dispatched = self
            .link
            .dispatch(self.server, session_id, method, params)?;
        Ok(PendingPublicCall::unified(self.server, method, dispatched))
    }

    /// Reference `AppServerSession.respond_to_callback` and
    /// `reject_callback`: the session takes the answer as a
    /// `callback/result`.
    pub(super) fn answer(
        &mut self,
        session_id: &str,
        callback_id: &str,
        output: Value,
        rejection: Option<&str>,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        let result = match rejection {
            Some(message) => json!({"callbackId": callback_id, "error": {"message": message}}),
            None => json!({"callbackId": callback_id, "output": output}),
        };
        self.call(
            session_id,
            "callback/result",
            json!({"sessionId": session_id, "result": result}),
        )
    }
}

impl SessionBackend for UnifiedSessions<'_> {
    fn view(&mut self, session_id: &str) -> Result<SessionView, ClientError> {
        let history = self.link.history(session_id)?;
        let mut view = self.server.session(session_id)?;
        view.snapshot = Some(vibe_core::events::ProjectionSnapshot {
            session_id: session_id.to_owned(),
            turn_id: None,
            handoff_cause: None,
            watermark: 0,
            lifecycle: vibe_core::events::LifecycleState::Idle,
            title: None,
            history,
        });
        Ok(view)
    }

    /// Reference `runtime.stats` on a unified session: what its last
    /// `session/statsUpdated` carried, and its runtime before that.
    fn stats(&mut self, session_id: &str) -> Result<(Value, u64), ClientError> {
        if let Some(accounting) = self.link.accounting(session_id) {
            return Ok(accounting);
        }
        let mut result = self.call(session_id, "runtime/read", json!({"sessionId": session_id}))?;
        let runtime = result.remove("runtime").unwrap_or(Value::Null);
        Ok((
            runtime.get("stats").cloned().unwrap_or(Value::Null),
            runtime
                .get("contextWindow")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        ))
    }

    fn published(&mut self, session_id: &str, field: &str) -> Option<Value> {
        self.link.published(session_id, field)
    }

    /// Reference `AppServerSession.act` up to its first event: `turn/start`,
    /// after which the session runs the turn on its own.
    fn reserve(
        &mut self,
        session_id: &str,
        turn: &TurnRequest,
        _images: PreparedImages,
    ) -> Result<TurnReservation, ClientError> {
        let result = self.call(
            session_id,
            "turn/start",
            json!({
                "sessionId": session_id,
                "message": turn.input,
                "injected": turn.injected,
                "clientUserMessageId": turn.client_user_message_id,
                "autoTitle": turn.auto_title,
                "userDisplayContent": turn.user_display_content,
                "mentionStats": turn.mention_stats,
            }),
        )?;
        let turn_id = result
            .get("turn")
            .and_then(|started| started.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| ClientError::InvalidResponse("missing turnId".to_owned()))?
            .to_owned();
        self.link.listen(&turn_id);
        Ok(self
            .server
            .unified_reservation(session_id, &turn_id, turn)?)
    }

    /// A unified session fires its loops on its own.
    fn reserve_due_loop<'a>(
        &'a mut self,
        _session_id: &'a str,
        _now_seconds: u64,
    ) -> ClientFuture<'a, Option<ScheduledTurn>> {
        Box::pin(async { Ok(None) })
    }

    fn finish(
        &mut self,
        reservation: &TurnReservation,
        outcome: TurnOutcome,
    ) -> Result<ProgrammaticTurn, ClientError> {
        ProgrammaticTurn::settled(&reservation.turn_id, outcome)
    }

    /// A unified session settles its own turns; one the client gave up on
    /// before it ended is interrupted.
    fn fail(
        &mut self,
        reservation: &TurnReservation,
        _error: PublicError,
    ) -> Result<(), ClientError> {
        // A turn that settled meanwhile has nothing left to interrupt.
        let _ = self.interrupt(&reservation.session_id, &reservation.turn_id);
        Ok(())
    }

    fn interrupt(
        &mut self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<InterruptOutcome, ClientError> {
        self.call(
            session_id,
            "turn/interrupt",
            json!({"sessionId": session_id, "expectedTurnId": turn_id}),
        )?;
        Ok(InterruptOutcome::Complete)
    }

    fn stop(&mut self, session_id: &str, turn_id: &str) -> Result<(), ClientError> {
        self.interrupt(session_id, turn_id).map(drop)
    }

    fn steer(
        &mut self,
        session_id: &str,
        turn_id: &str,
        input: &[PublicContentBlock],
        client_user_message_id: Option<&str>,
    ) -> Result<(), ClientError> {
        self.call(
            session_id,
            "turn/steer",
            json!({
                "sessionId": session_id,
                "expectedTurnId": turn_id,
                "message": input,
                "clientUserMessageId": client_user_message_id,
            }),
        )
        .map(drop)
    }

    fn inject_context(&mut self, session_id: &str, text: &str) -> Result<(), ClientError> {
        self.call(
            session_id,
            "session/context/inject",
            json!({"sessionId": session_id, "input": [{"type": "text", "text": text}]}),
        )
        .map(drop)
    }

    fn compact<'a>(
        &'a mut self,
        session_id: &'a str,
        extra_instructions: &'a str,
    ) -> ClientFuture<'a, BTreeMap<String, Value>> {
        Box::pin(async move {
            self.link
                .dispatch(
                    self.server,
                    session_id,
                    "session/compact",
                    json!({"sessionId": session_id, "extraInstructions": extra_instructions}),
                )?
                .result()
                .await
        })
    }

    fn close<'a>(&'a mut self, session_id: &'a str) -> ClientFuture<'a, ()> {
        Box::pin(async move {
            self.link.close(self.server, session_id).await;
            Ok(())
        })
    }
}

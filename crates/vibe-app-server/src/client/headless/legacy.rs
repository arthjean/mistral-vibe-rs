//! The sessions the driver runs behind the in-process connection: the
//! server reserves and settles each turn, the driver runs it, and the
//! questions its interactive tools ask are raised as callbacks here.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use vibe_core::engine::TurnOutcome;
use vibe_core::events::{CallbackKind as EngineCallbackKind, ProjectionReducer};
use vibe_protocol::{Envelope, decode_frame};

use super::{ClientFuture, SessionBackend, TurnUpdates};
use crate::client::in_process::{ServerProjectionObserver, public_history_entry_identity};
use crate::client::interactive::InteractiveCallbackRequest;
use crate::client::interactive::queue::{CallbackAnswer, CallbackQueue};
use crate::client::{
    AppServer, ClientError, DeferredWork, DriverError, InProcessClient, InterruptOutcome,
    MAX_PROGRAMMATIC_UPDATES, PreparedImages, ProgrammaticTurn, ProgrammaticUpdate,
    PublicContentBlock, PublicError, PublicHistoryEntry, ScheduledTurn, ServerError, SessionView,
    TurnDriver, TurnRequest, TurnReservation, provider_images, request_bytes, response_frame,
    response_result, turn_error_code,
};

pub(super) struct LegacySessions<'a, D> {
    pub(super) client: &'a mut InProcessClient,
    pub(super) driver: &'a Arc<D>,
    pub(super) callbacks: &'a mut CallbackQueue,
    pub(super) requests: &'a mut Option<tokio::sync::mpsc::Receiver<InteractiveCallbackRequest>>,
}

impl<D> LegacySessions<'_, D>
where
    D: TurnDriver,
{
    /// A root session that titles itself learns whether a fast model serves
    /// its titles before it is handed back, as the reference's session open
    /// waits on the probe.
    pub(super) fn probe_for_titles(&self, session_id: &str) {
        if self.client.session_titles_itself(session_id) {
            self.driver.probe_utility_models();
        }
    }

    pub(super) fn call(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        let requested = params
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let result = self.client.public_call(method, params)?;
        // A resumed or continued session opens a root of its own, which the
        // reference builds, probe included; a fork copies its source and
        // does not probe (`vibe/app_server/_runtime.py:1073`).
        if matches!(
            method,
            "session/resume"
                | "session/continue"
                | "internal/session/resume"
                | "internal/session/continue"
        ) && let Some(session_id) = result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(requested)
        {
            self.probe_for_titles(&session_id);
        }
        Ok(result)
    }

    /// What the interactive tools asked since the last drain, received.
    fn incoming(&mut self) -> Vec<InteractiveCallbackRequest> {
        let mut incoming = Vec::new();
        if let Some(receiver) = self.requests.as_mut() {
            while let Ok(request) = receiver.try_recv() {
                incoming.push(request);
            }
        }
        incoming
    }

    /// Raises every question the interactive tools asked that can be raised,
    /// and answers the callback entries they became.
    pub(super) fn raise_callbacks(&mut self) -> Vec<PublicHistoryEntry> {
        let incoming = self.incoming();
        let server = self.client.server.clone();
        let client = &mut *self.client;
        self.callbacks.raise(
            &server,
            self.driver.as_ref(),
            incoming,
            |session_id, turn_id, kind, title, detail| {
                deliver_callback(client, session_id, turn_id, kind, title, detail)
                    .map_err(|error| error.to_string())
            },
        )
    }

    /// Fails the questions of `session_id` (of every session when `None`)
    /// raised in `turn_id` (in any turn when `None`), and those still waiting.
    pub(super) fn fail_callbacks(
        &mut self,
        session_id: Option<&str>,
        turn_id: Option<&str>,
        message: &str,
    ) {
        let incoming = self.incoming();
        self.callbacks.fail(session_id, turn_id, message, incoming);
    }

    /// Settles `callback_id` with the client's `params`: the server records
    /// the answer, the driver resumes what waited on it, and an interactive
    /// tool that asked it reads `output`, or `rejection` when the client
    /// refused the question.
    pub(super) fn answer(
        &mut self,
        callback_id: &str,
        params: Value,
        output: Value,
        rejection: Option<&str>,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        let request_id = self.client.take_request_id();
        let request = request_bytes(request_id.clone(), "callback/respond", params)?;
        let batch = self.client.connection.dispatch(&request);
        if batch.close_after_flush {
            return Err(ClientError::InvalidResponse(
                "callback response closed the connection".to_owned(),
            ));
        }
        let result = response_result(response_frame(batch.outbound)?, &request_id)?;
        for work in batch.deferred {
            let driver_result = match work {
                DeferredWork::ResolveCallback {
                    session_id,
                    turn_id,
                    callback_id: deferred_callback_id,
                    accepted,
                    value,
                } if deferred_callback_id == callback_id => self.driver.resolve_callback(
                    &session_id,
                    &turn_id,
                    &deferred_callback_id,
                    accepted,
                    value.as_deref(),
                ),
                DeferredWork::InterruptTurn {
                    session_id,
                    turn_id,
                } => self.driver.interrupt(&session_id, &turn_id),
                _ => {
                    let message = "callback response returned unexpected deferred work";
                    self.callbacks.fail_callback(callback_id, message);
                    return Err(ClientError::InvalidResponse(message.to_owned()));
                }
            };
            match driver_result {
                Ok(()) | Err(DriverError::UnsupportedControl("callback/respond")) => {}
                Err(error) => {
                    self.callbacks
                        .fail_callback(callback_id, &error.to_string());
                    return Err(ClientError::Driver(error));
                }
            }
        }
        let answer = match rejection {
            Some(message) => CallbackAnswer::Refused(message.to_owned()),
            None => CallbackAnswer::Output(output),
        };
        self.callbacks.answer(callback_id, answer)?;
        Ok(result)
    }
}

/// Raises one callback on the in-process connection and answers its
/// identifier and the entry the client reads it as. The delivery is
/// acknowledged at once, as a client acknowledges a `callback/call`.
fn deliver_callback(
    client: &mut InProcessClient,
    session_id: &str,
    turn_id: &str,
    kind: EngineCallbackKind,
    title: String,
    detail: Value,
) -> Result<(String, PublicHistoryEntry), ClientError> {
    let (callback_id, frames) = client
        .connection
        .request_callback_with_detail(session_id, turn_id, kind, title, detail)?;
    // The delivery is the last frame; the status update before it is for
    // the wire, not for this in-process caller.
    let request_bytes = frames
        .last()
        .cloned()
        .ok_or_else(|| ClientError::InvalidResponse("callback was not delivered".to_owned()))?;
    let request = match decode_frame(&request_bytes).map_err(ServerError::Protocol)? {
        Envelope::Request(request) if request.method == "callback/call" => request,
        _ => {
            return Err(ClientError::InvalidResponse(
                "callback delivery was not a callback/call request".to_owned(),
            ));
        }
    };
    let callback = request
        .params
        .get("callback")
        .cloned()
        .ok_or_else(|| {
            ClientError::InvalidResponse("callback delivery omitted callback".to_owned())
        })
        .and_then(|value| {
            serde_json::from_value::<PublicHistoryEntry>(value).map_err(ClientError::Json)
        })?;
    let acknowledgment = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": request.id,
        "result": {
            "callbackId": callback_id,
            "accepted": true,
        },
    }))
    .map_err(ClientError::Json)?;
    let batch = client.connection.dispatch(&acknowledgment);
    if batch.close_after_flush || !batch.outbound.is_empty() || !batch.deferred.is_empty() {
        return Err(ClientError::InvalidResponse(
            "callback delivery acknowledgment was rejected".to_owned(),
        ));
    }
    Ok((callback_id, callback))
}

/// A turn of `session_id` read through the server's projection of it, seeded
/// with what the session already holds.
pub(super) fn turn_updates(
    server: &AppServer,
    session_id: &str,
    turn_id: &str,
) -> Result<TurnUpdates, ClientError> {
    let seed = server.live_projection_seed(session_id, turn_id)?;
    // The turn is starting: its user message is a step of the session's
    // accounting, which the transport records when it announces the turn.
    server.turn_started(session_id, turn_id)?;
    let mut reducer = ProjectionReducer::for_turn(session_id, turn_id);
    reducer
        .restore(seed.clone(), 0)
        .map_err(|error| ClientError::InvalidResponse(error.to_string()))?;
    let emitted = seed
        .history
        .iter()
        .map(|entry| {
            serde_json::to_value(entry)
                .map(|encoded| (public_history_entry_identity(entry), encoded))
                .map_err(ClientError::Json)
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let (sender, receiver) =
        tokio::sync::mpsc::channel::<ProgrammaticUpdate>(MAX_PROGRAMMATIC_UPDATES);
    Ok((
        Arc::new(ServerProjectionObserver {
            server: server.clone(),
            session_id: session_id.to_owned(),
            turn_id: turn_id.to_owned(),
            reducer: Mutex::new(reducer),
            emitted: Mutex::new(emitted),
            sender,
        }),
        receiver,
    ))
}

impl<D> SessionBackend for LegacySessions<'_, D>
where
    D: TurnDriver,
{
    fn view(&mut self, session_id: &str) -> Result<SessionView, ClientError> {
        self.client.session(session_id)
    }

    fn stats(&mut self, session_id: &str) -> Result<(Value, u64), ClientError> {
        let mut result = self.call("stats/read", json!({"sessionId": session_id}))?;
        Ok((
            result.remove("stats").unwrap_or(Value::Null),
            result
                .get("contextWindow")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        ))
    }

    /// A legacy session publishes no state of its own: the server holds it.
    fn published(&mut self, _session_id: &str, _field: &str) -> Option<Value> {
        None
    }

    fn reserve(
        &mut self,
        session_id: &str,
        turn: &TurnRequest,
        images: PreparedImages,
    ) -> Result<TurnReservation, ClientError> {
        let mut reservation = self.client.reserve_turn_request(session_id, turn)?;
        reservation.prepared_images = Some(images);
        Ok(reservation)
    }

    fn reserve_due_loop<'a>(
        &'a mut self,
        session_id: &'a str,
        now_seconds: u64,
    ) -> ClientFuture<'a, Option<ScheduledTurn>> {
        Box::pin(async move {
            self.client.configure_pending_mcp(session_id).await?;
            let Some(mut scheduled) = self.client.reserve_due_loop(session_id, now_seconds)? else {
                return Ok(None);
            };
            match provider_images(&scheduled.reservation.input).await {
                Ok(images) => scheduled.reservation.prepared_images = Some(images),
                Err(error) => {
                    self.client.fail_turn(
                        &scheduled.reservation,
                        &error.to_string(),
                        turn_error_code(&error),
                    )?;
                    return Err(ClientError::Driver(error));
                }
            }
            Ok(Some(scheduled))
        })
    }

    fn finish(
        &mut self,
        reservation: &TurnReservation,
        outcome: TurnOutcome,
    ) -> Result<ProgrammaticTurn, ClientError> {
        let result = self.client.finish_turn(reservation, outcome);
        self.fail_callbacks(
            Some(&reservation.session_id),
            Some(&reservation.turn_id),
            "turn completed before the callback was resolved",
        );
        result
    }

    fn fail(
        &mut self,
        reservation: &TurnReservation,
        error: PublicError,
    ) -> Result<(), ClientError> {
        let message = error.message.clone();
        let result = self.client.fail_turn_with(reservation, error);
        self.fail_callbacks(
            Some(&reservation.session_id),
            Some(&reservation.turn_id),
            &message,
        );
        result
    }

    fn interrupt(
        &mut self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<InterruptOutcome, ClientError> {
        self.driver.interrupt(session_id, turn_id)?;
        if let Err(canonical_error) = self.client.interrupt(session_id, turn_id) {
            return Ok(InterruptOutcome::DriverOnly { canonical_error });
        }
        self.fail_callbacks(Some(session_id), Some(turn_id), "turn was interrupted");
        Ok(InterruptOutcome::Complete)
    }

    fn stop(&mut self, session_id: &str, turn_id: &str) -> Result<(), ClientError> {
        Ok(self.driver.interrupt(session_id, turn_id)?)
    }

    fn steer(
        &mut self,
        session_id: &str,
        turn_id: &str,
        input: &[PublicContentBlock],
        client_user_message_id: Option<&str>,
    ) -> Result<(), ClientError> {
        let (content, inject_invoked_skill) =
            self.client
                .steer(session_id, turn_id, input, client_user_message_id)?;
        self.driver
            .steer(session_id, turn_id, &content, inject_invoked_skill)?;
        Ok(())
    }

    fn inject_context(&mut self, session_id: &str, text: &str) -> Result<(), ClientError> {
        if let Some((content, as_message, inject_invoked_skill)) =
            self.client.inject_context(session_id, text)?
        {
            self.driver
                .inject_context(session_id, &content, as_message, inject_invoked_skill)?;
        }
        Ok(())
    }

    fn compact<'a>(
        &'a mut self,
        session_id: &'a str,
        extra_instructions: &'a str,
    ) -> ClientFuture<'a, BTreeMap<String, Value>> {
        Box::pin(async move {
            let (request_id, canonical_session_id, working_directory) = self
                .client
                .reserve_compaction(session_id, extra_instructions)?;
            let result = self
                .driver
                .compact(
                    &canonical_session_id,
                    &working_directory,
                    extra_instructions,
                )
                .await;
            self.client
                .finish_compaction(request_id, &canonical_session_id, result)
        })
    }

    fn close<'a>(&'a mut self, session_id: &'a str) -> ClientFuture<'a, ()> {
        Box::pin(async move {
            let interrupt = self.client.close_session(session_id).await?;
            self.fail_callbacks(Some(session_id), None, "session was closed");
            if let Some((canonical_session_id, turn_id)) = interrupt {
                if canonical_session_id != session_id {
                    self.fail_callbacks(Some(&canonical_session_id), None, "session was closed");
                }
                self.driver.interrupt(&canonical_session_id, &turn_id)?;
            }
            Ok(())
        })
    }
}

//! The requests a connection puts to its client, and the answers it reads
//! back: callbacks a turn waits on, raised, replayed to a client that
//! attaches, and settled by the client's response.

use super::*;

impl ServerConnection {
    pub fn request_callback(
        &mut self,
        session_id: &str,
        turn_id: &str,
        kind: EngineCallbackKind,
        prompt: impl Into<String>,
    ) -> Result<(String, Vec<Vec<u8>>), ServerError> {
        let prompt = prompt.into();
        self.deliver_callback(session_id, turn_id, kind, |server| {
            server.request_callback(session_id, turn_id, kind, prompt)
        })
    }

    pub fn request_callback_with_detail(
        &mut self,
        session_id: &str,
        turn_id: &str,
        kind: EngineCallbackKind,
        title: impl Into<String>,
        detail: Value,
    ) -> Result<(String, Vec<Vec<u8>>), ServerError> {
        let title = title.into();
        self.deliver_callback(session_id, turn_id, kind, |server| {
            server.request_callback_with_detail(session_id, turn_id, kind, title, detail)
        })
    }

    /// The frames a completed attachment puts on the wire.
    ///
    /// A newly attached client is handed the whole session as a sequenced
    /// `session/snapshot`, then any callback still open is replayed to it: the
    /// question was delivered to whoever was attached before, so without the
    /// replay the new client would wait on a turn it cannot unblock.
    pub(crate) fn attachment_frames(&mut self, session_id: &str) -> Vec<Vec<u8>> {
        let mut sessions = self.server.lock_sessions();
        let Some(session) = sessions.get_mut(session_id) else {
            return Vec::new();
        };
        let mut frames = vec![session_snapshot_frame(session)];
        let open = session
            .pending_callback
            .clone()
            .zip(session.active_turn.clone())
            .map(|(callback, turn_id)| (session.id.clone(), callback, turn_id));
        drop(sessions);
        let Some((canonical_session_id, callback, turn_id)) = open else {
            return frames;
        };
        // A kind this connection cannot answer is not replayed to it.
        if !self.answers_callback(callback.kind) {
            return frames;
        }
        let sequence = self.server.next_callback.fetch_add(1, Ordering::Relaxed);
        let request_id = RequestId::Integer(i64::try_from(sequence).unwrap_or(i64::MAX));
        self.pending_server_requests.insert(
            request_id.clone(),
            CallbackRoute {
                session_id: canonical_session_id,
                turn_id,
                callback_id: callback.id.clone(),
                answered: false,
            },
        );
        frames.push(encode_frame(&Envelope::Request(ServerRequest {
            jsonrpc: JsonRpcVersion::V2,
            id: request_id,
            method: "callback/call".to_owned(),
            params: result_map([("callback", json!(callback.entry))]),
        })));
        frames
    }

    /// Whether this connection's client answers callbacks of `kind`. A
    /// connector authorization is put to none: the reference refuses to raise
    /// one rather than emit a callback the client has no way to close.
    fn answers_callback(&self, kind: EngineCallbackKind) -> bool {
        let supported = match kind {
            EngineCallbackKind::Approval => CallbackKind::Approval,
            EngineCallbackKind::UserInput => CallbackKind::UserInput,
            EngineCallbackKind::ConnectorAuth => return false,
        };
        self.capabilities.callback_kinds.contains(&supported)
    }

    /// Mints a callback through `request` and routes the client's answer back
    /// to the turn that is waiting for it.
    fn deliver_callback(
        &mut self,
        session_id: &str,
        turn_id: &str,
        kind: EngineCallbackKind,
        request: impl FnOnce(&AppServer) -> Result<(String, Vec<Vec<u8>>), ServerError>,
    ) -> Result<(String, Vec<Vec<u8>>), ServerError> {
        if self.state != ConnectionState::Ready {
            return Err(ServerError::NotInitialized);
        }
        if !self.answers_callback(kind) {
            return Err(ServerError::UnsupportedClientCallbackKind(kind));
        }
        let (callback_id, frames) = request(&self.server)?;
        // The delivery itself is the last frame: the status update precedes it.
        let delivery = frames.last().ok_or(ServerError::InvalidCallbackRequest)?;
        let request_id = match decode_frame(delivery)? {
            Envelope::Request(request) => request.id,
            _ => return Err(ServerError::InvalidCallbackRequest),
        };
        self.pending_server_requests.insert(
            request_id,
            CallbackRoute {
                session_id: session_id.to_owned(),
                turn_id: turn_id.to_owned(),
                callback_id: callback_id.clone(),
                answered: false,
            },
        );
        Ok((callback_id, frames))
    }

    pub(super) fn handle_server_success(&mut self, response: SuccessResponse) -> DispatchBatch {
        if self.server.client_tools.resolve(
            &response.id,
            Ok(Value::Object(response.result.clone().into_iter().collect())),
        ) {
            return DispatchBatch::empty();
        }
        let Some(route) = self.pending_server_requests.remove(&response.id) else {
            return self.close_for_protocol_error();
        };
        let callback_id_matches = response.result.get("callbackId").and_then(Value::as_str)
            == Some(route.callback_id.as_str());
        let accepted = response
            .result
            .get("accepted")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if !callback_id_matches {
            return self.close_for_protocol_error();
        }
        if accepted || route.answered {
            DispatchBatch::empty()
        } else {
            match self
                .server
                .reject_callback(&route, "Client did not accept callback delivery")
            {
                Ok((status, deferred)) => DispatchBatch {
                    outbound: rejection_frames(status),
                    deferred,
                    close_after_flush: false,
                },
                Err(_) => self.close_for_protocol_error(),
            }
        }
    }

    pub(super) fn handle_server_error(&mut self, response: ErrorResponse) -> DispatchBatch {
        if self
            .server
            .client_tools
            .resolve(&response.id, Err(response.error.message.clone()))
        {
            return DispatchBatch::empty();
        }
        let Some(route) = self.pending_server_requests.remove(&response.id) else {
            return self.close_for_protocol_error();
        };
        if route.answered {
            return DispatchBatch::empty();
        }
        match self.server.reject_callback(&route, &response.error.message) {
            Ok((status, deferred)) => DispatchBatch {
                outbound: rejection_frames(status),
                deferred,
                close_after_flush: false,
            },
            Err(_) => self.close_for_protocol_error(),
        }
    }

    pub(super) fn close_for_protocol_error(&mut self) -> DispatchBatch {
        self.state = ConnectionState::Closed;
        DispatchBatch {
            outbound: Vec::new(),
            deferred: Vec::new(),
            close_after_flush: true,
        }
    }
}

//! The Unified Harness, reached by a client in this process.
//!
//! Reference `LocalHarness` (`vibe/app_server/local.py`) and
//! `AppServerSession` (`vibe/app_server/session.py`): the terminal, the
//! programmatic launch and the editor bridge open their sessions on the app
//! server over an in-memory transport and read them through the events it
//! publishes, whichever backend serves them. Here each unified session gets a
//! connection of its own, as the reference's host opens one per session, so
//! every method reaches it through the routing a stdio client takes. What the
//! session publishes arrives on one frame channel, and a pump folds it into a
//! mirror of each session (reference `ClientProjection` in
//! `vibe/app_server/events.py`), hands each turn's entries to whoever runs the
//! turn, and keeps the callbacks it asks for until the client drains them.

use std::sync::atomic::AtomicI64;

use tokio::sync::{Notify, mpsc, oneshot};

use super::*;

/// A session a client opened: its identifier, the answer that opened it and
/// the work that answer deferred.
pub(super) type Opened = (String, BTreeMap<String, Value>, Vec<DeferredWork>);

mod mirror;

use mirror::{Mirror, Published};

/// What the pump and the service share.
#[derive(Default)]
struct Hub {
    sessions: BTreeMap<String, Mirror>,
    /// The channel each running turn's entries go to, by turn.
    listeners: BTreeMap<String, tokio::sync::mpsc::Sender<ProgrammaticUpdate>>,
    /// The receiving end of a turn's channel until its runner takes it.
    parked: BTreeMap<String, tokio::sync::mpsc::Receiver<ProgrammaticUpdate>>,
    /// Turns that settled, by turn, as `turn/completed` published them.
    settled: BTreeMap<String, Value>,
    /// Answers a session published after its request returned, by request.
    answers: BTreeMap<String, oneshot::Sender<Vec<u8>>>,
    /// What a turn published before anyone listened to it, by turn.
    orphans: BTreeMap<String, Vec<ProgrammaticUpdate>>,
    /// Callbacks a session asked for and the client has not drained.
    callbacks: VecDeque<Value>,
    /// The session each callback belongs to.
    callback_sessions: BTreeMap<String, String>,
    next_update: u64,
}

pub(super) struct Shared {
    hub: Mutex<Hub>,
    changed: Notify,
}

impl Shared {
    /// Folds one frame a unified session published into the mirrors.
    fn receive(&self, server: &AppServer, frame: Vec<u8>) {
        let Ok(envelope) = decode_frame(&frame) else {
            return;
        };
        let mut hub = lock(&self.hub);
        match envelope {
            Envelope::Notification(notification) => {
                hub.publish(&notification.method, &notification.params);
            }
            Envelope::Request(request) if request.method == "callback/call" => {
                let callback = request
                    .params
                    .get("callback")
                    .cloned()
                    .unwrap_or(Value::Null);
                // Reference `_handle_callback_request`: the delivery is
                // acknowledged at once, and the answer follows as its own
                // `callback/result`.
                server.client_tools.resolve(
                    &request.id,
                    Ok(json!({"callbackId": callback["callbackId"].as_str().unwrap_or_default()})),
                );
                hub.ask(callback);
            }
            Envelope::Success(SuccessResponse { ref id, .. })
            | Envelope::Error(ErrorResponse { ref id, .. }) => {
                if let Some(answer) = hub.answers.remove(&request_key(id)) {
                    let _ = answer.send(frame);
                }
            }
            Envelope::Request(_) => {}
        }
        drop(hub);
        self.changed.notify_waiters();
    }
}

impl Hub {
    /// Folds one notification into the mirror of the session that published
    /// it, and hands what it changed to the turn reading that session. Every
    /// event the session numbers reaches that turn, as an entry or else as a
    /// watermark; one no turn reads yet waits for the next that does.
    fn publish(&mut self, method: &str, params: &BTreeMap<String, Value>) {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some(mirror) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let running = mirror.active_turn();
        let seen = mirror.last_event;
        let published = mirror.consume(method, params);
        let event_id = params.get("eventId").and_then(Value::as_u64);
        let reader = mirror.active_turn().or_else(|| running.clone());
        let renamed = (method == "session/updated"
            && params
                .get("patch")
                .and_then(Value::as_array)
                .is_some_and(|patch| {
                    patch
                        .iter()
                        .any(|op| matches!(op["path"].as_str(), Some("/title" | "/preview")))
                }))
        .then(|| mirror.renamed());
        let mut skipped = std::mem::take(&mut mirror.skipped);
        if let (Some(update), Some(turn_id)) = (renamed, &reader) {
            self.deliver(turn_id, update);
        }
        let delivered = self.route(published, event_id, running, &mut skipped);
        if let Some(event_id) = event_id.filter(|event_id| *event_id > seen && !delivered) {
            match reader.filter(|turn_id| self.follows(turn_id)) {
                Some(turn_id) => {
                    self.flush_skipped(&turn_id, &mut skipped);
                    self.deliver(
                        &turn_id,
                        ProgrammaticUpdate::Watermark {
                            event_id,
                            emitted_at: 0,
                        },
                    );
                }
                None => skipped.push(event_id),
            }
        }
        if let Some(mirror) = self.sessions.get_mut(&session_id) {
            let excess = skipped.len().saturating_sub(MAX_PROGRAMMATIC_UPDATES);
            skipped.drain(..excess);
            mirror.skipped = skipped;
        }
    }

    /// Hands what one notification changed to the turn it concerns, the one
    /// `running` when it names none, and answers whether an entry reached a
    /// reader.
    fn route(
        &mut self,
        published: Published,
        event_id: Option<u64>,
        running: Option<String>,
        skipped: &mut Vec<u64>,
    ) -> bool {
        match published {
            Published::Entry(entry) => {
                let Ok(parsed) = serde_json::from_value::<PublicHistoryEntry>(entry.clone()) else {
                    return false;
                };
                let turn_id = entry["turnId"].as_str().map(ToOwned::to_owned).or(running);
                let event_id = event_id.unwrap_or_else(|| {
                    self.next_update += 1;
                    self.next_update
                });
                let Some(turn_id) = turn_id.filter(|turn_id| self.follows(turn_id)) else {
                    return false;
                };
                self.flush_skipped(&turn_id, skipped);
                self.deliver(
                    &turn_id,
                    ProgrammaticUpdate::HistoryEntry {
                        event_id,
                        emitted_at: 0,
                        entry: Box::new(parsed),
                    },
                );
                return true;
            }
            Published::Stats {
                stats,
                context_window,
            } => {
                if let Some(turn_id) = running {
                    self.deliver(
                        &turn_id,
                        ProgrammaticUpdate::Stats {
                            context_tokens: stats.context_tokens,
                            input_tokens: stats.session_prompt_tokens,
                            output_tokens: stats.session_completion_tokens,
                        },
                    );
                    self.deliver(
                        &turn_id,
                        ProgrammaticUpdate::Accounting {
                            stats,
                            context_window,
                        },
                    );
                }
            }
            Published::TurnCompleted(turn) => {
                if let Some(turn_id) = turn["id"].as_str() {
                    self.listeners.remove(turn_id);
                    if let Some(orphans) = self.orphans.remove(turn_id) {
                        // Kept for a runner that listens after the turn
                        // already ended.
                        let (sender, receiver) =
                            tokio::sync::mpsc::channel(MAX_PROGRAMMATIC_UPDATES);
                        for update in orphans {
                            let _ = sender.try_send(update);
                        }
                        self.parked.insert(turn_id.to_owned(), receiver);
                    }
                    self.settled.insert(turn_id.to_owned(), turn);
                }
            }
            Published::Handoff { from, to } => {
                if let Some(mirror) = self.sessions.remove(&from) {
                    self.sessions.insert(to, mirror);
                }
            }
            Published::Nothing => {}
        }
        false
    }

    /// Keeps a callback a session asked for until the client drains it.
    fn ask(&mut self, callback: Value) {
        let callback_id = callback["callbackId"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let session_id = callback["sessionId"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.callback_sessions.insert(callback_id, session_id);
        self.callbacks.push_back(callback);
    }

    /// Whether `turn_id` still has a reader to hand updates to, now or once
    /// it listens.
    fn follows(&self, turn_id: &str) -> bool {
        !self.settled.contains_key(turn_id)
    }

    /// Hands `turn_id` the events no turn took before it, in order.
    fn flush_skipped(&mut self, turn_id: &str, skipped: &mut Vec<u64>) {
        for event_id in skipped.drain(..) {
            self.deliver(
                turn_id,
                ProgrammaticUpdate::Watermark {
                    event_id,
                    emitted_at: 0,
                },
            );
        }
    }

    /// Hands `update` to whoever runs `turn_id`, or keeps it until someone
    /// listens.
    fn deliver(&mut self, turn_id: &str, update: ProgrammaticUpdate) {
        match self.listeners.get(turn_id) {
            Some(sender) => {
                let _ = sender.try_send(update);
            }
            None if !self.settled.contains_key(turn_id) => {
                let orphans = self.orphans.entry(turn_id.to_owned()).or_default();
                if orphans.len() < MAX_PROGRAMMATIC_UPDATES {
                    orphans.push(update);
                }
            }
            None => {}
        }
    }
}

/// Whether a unified session's stream carries what `work` publishes, its
/// answer included, rather than the call that deferred it.
fn streams(work: &DeferredWork) -> bool {
    matches!(
        work,
        DeferredWork::AttachUnified { .. }
            | DeferredWork::RunUnifiedTurn(_)
            | DeferredWork::ReleaseUnifiedCallback { .. }
            | DeferredWork::ShellCommand { .. }
            | DeferredWork::ShellInterrupt { .. }
    )
}

fn request_key(id: &RequestId) -> String {
    serde_json::to_string(id).unwrap_or_default()
}

/// The unified sessions one client opened, and the connection each runs on.
pub(super) struct UnifiedLink {
    client_info: ClientInfo,
    capabilities: ClientCapabilities,
    connections: BTreeMap<String, ServerConnection>,
    frames: mpsc::UnboundedSender<Vec<u8>>,
    receiver: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
    pub(super) shared: Arc<Shared>,
    next_request: AtomicI64,
}

impl UnifiedLink {
    pub(super) fn new(client_info: ClientInfo, capabilities: ClientCapabilities) -> Self {
        let (frames, receiver) = mpsc::unbounded_channel();
        Self {
            client_info,
            capabilities,
            connections: BTreeMap::new(),
            frames,
            receiver: Some(receiver),
            shared: Arc::new(Shared {
                hub: Mutex::new(Hub::default()),
                changed: Notify::new(),
            }),
            next_request: AtomicI64::new(1),
        }
    }

    /// Whether `session_id` is one of the unified sessions this link opened.
    pub(super) fn holds(&self, session_id: &str) -> bool {
        self.connections.contains_key(session_id)
            || self
                .connections
                .values()
                .any(|connection| connection.root_session_id().as_deref() == Some(session_id))
    }

    fn take_request_id(&self) -> RequestId {
        RequestId::Integer(self.next_request.fetch_add(1, Ordering::Relaxed))
    }

    /// Starts the task that reads what the sessions publish, once a runtime
    /// is there to run it.
    fn ensure_pump(&mut self, server: &AppServer) {
        if self.receiver.is_none() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let Some(mut receiver) = self.receiver.take() else {
            return;
        };
        let shared = Arc::clone(&self.shared);
        let server = server.clone();
        handle.spawn(async move {
            while let Some(frame) = receiver.recv().await {
                shared.receive(&server, frame);
            }
        });
    }

    /// A connection of its own for the next session, initialized as the
    /// client that owns this link.
    fn connect(&self, server: &AppServer) -> Result<ServerConnection, ClientError> {
        let mut connection = server.connect(TransportKind::InProcess);
        let request_id = self.take_request_id();
        let request = request_bytes(
            request_id.clone(),
            "initialize",
            json!({
                "clientInfo": self.client_info,
                "capabilities": self.capabilities,
            }),
        )?;
        let batch = connection.dispatch(&request);
        response_result(response_frame(batch.outbound)?, &request_id)?;
        let initialized = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }))
        .map_err(ClientError::Json)?;
        connection.dispatch(&initialized);
        Ok(connection)
    }

    /// Reference `AppServerSession.open` on a new session: `session/start`
    /// with the options a launch resolved. Answers the session and the MCP
    /// servers it still has to configure.
    pub(super) fn start(
        &mut self,
        server: &AppServer,
        options: &SessionOptions,
    ) -> Result<(String, Vec<McpServerConfig>), ClientError> {
        let params = serde_json::to_value(options).map_err(ClientError::Json)?;
        let (session_id, _, deferred) = self.open(server, "session/start", params)?;
        let mut mcp = Vec::new();
        for work in deferred {
            if let DeferredWork::ConfigureMcp { configs, .. } = work {
                mcp = configs;
            }
        }
        Ok((session_id, mcp))
    }

    /// Opens a session on a fresh connection with `method` (a start, a resume
    /// or a continuation), registers it and starts reading it.
    pub(super) fn open(
        &mut self,
        server: &AppServer,
        method: &str,
        params: Value,
    ) -> Result<Opened, ClientError> {
        self.ensure_pump(server);
        let mut connection = self.connect(server)?;
        let request_id = self.take_request_id();
        let request = request_bytes(request_id.clone(), method, params)?;
        let batch = connection.dispatch(&request);
        let result = response_result(response_frame(batch.outbound)?, &request_id)?;
        let state = result.get("state").cloned().unwrap_or(Value::Null);
        let session_id = state["session"]["id"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| ClientError::InvalidResponse("missing sessionId".to_owned()))?;
        lock(&self.shared.hub)
            .sessions
            .insert(session_id.clone(), Mirror::new(state));
        let rest = self.settle(server, batch.deferred)?;
        self.connections.insert(session_id.clone(), connection);
        Ok((session_id, result, rest))
    }

    /// Runs the work a dispatch deferred that the session's own stream
    /// carries, its answer included, and hands back the rest.
    ///
    /// That work starts the session's loops or runs beside the client, so it
    /// needs a runtime: without one the dispatch fails rather than drop it.
    fn settle(
        &self,
        server: &AppServer,
        deferred: Vec<DeferredWork>,
    ) -> Result<Vec<DeferredWork>, ClientError> {
        let (streamed, rest): (Vec<_>, Vec<_>) = deferred.into_iter().partition(streams);
        if streamed.is_empty() {
            return Ok(rest);
        }
        let handle = tokio::runtime::Handle::try_current().map_err(|_| ClientError::NoRuntime)?;
        for work in streamed {
            let frames = self.frames.clone();
            let sink: FrameSink = Arc::new(move |frame| {
                let _ = frames.send(frame);
            });
            if let Execution::Pending(pending) = server.execute_deferred(work, sink) {
                let frames = self.frames.clone();
                handle.spawn(async move {
                    for frame in pending.await.outbound {
                        let _ = frames.send(frame);
                    }
                });
            }
        }
        Ok(rest)
    }

    /// The connection serving `session_id`, re-filed under it when a rewind
    /// or a cleared history moved its root to a new identifier.
    fn connection(&mut self, session_id: &str) -> Option<&mut ServerConnection> {
        if !self.connections.contains_key(session_id) {
            let moved = self
                .connections
                .iter()
                .find(|(_, connection)| connection.root_session_id().as_deref() == Some(session_id))
                .map(|(key, _)| key.clone())?;
            let connection = self.connections.remove(&moved)?;
            self.connections.insert(session_id.to_owned(), connection);
        }
        self.connections.get_mut(session_id)
    }

    /// Dispatches `method` on the connection of `session_id`.
    pub(super) fn dispatch(
        &mut self,
        server: &AppServer,
        session_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Dispatched, ClientError> {
        self.ensure_pump(server);
        let request_id = self.take_request_id();
        let request = request_bytes(request_id.clone(), method, params)?;
        let connection = self.connection(session_id).ok_or_else(|| {
            ClientError::Server(ServerError::SessionNotFound(session_id.to_owned()))
        })?;
        let batch = connection.dispatch(&request);
        let moved = connection.root_session_id();
        // What the answer published reaches the mirrors before anything the
        // work it deferred publishes.
        for frame in &batch.outbound {
            if matches!(decode_frame(frame), Ok(Envelope::Notification(_))) {
                self.shared.receive(server, frame.clone());
            }
        }
        let response = batch.outbound.iter().find(|frame| {
            matches!(
                decode_frame(frame),
                Ok(Envelope::Success(_) | Envelope::Error(_))
            )
        });
        // A request a session answers once its work ends, a compaction's, is
        // waited on before that work can run.
        // So is a manual command's, which answers once the command settled.
        let answered_later = batch.deferred.iter().any(|work| {
            matches!(work, DeferredWork::RunUnifiedTurn(work) if work.compaction.is_some())
                || matches!(
                    work,
                    DeferredWork::ShellCommand { .. } | DeferredWork::ShellInterrupt { .. }
                )
        });
        let later = match response {
            Some(frame) => {
                if let Ok(Envelope::Success(SuccessResponse { result, .. })) = decode_frame(frame)
                    && let Some(state) = result.get("state")
                    && let Some(id) = state["session"]["id"].as_str()
                    && id != session_id
                {
                    lock(&self.shared.hub)
                        .sessions
                        .insert(id.to_owned(), Mirror::new(state.clone()));
                }
                None
            }
            None if answered_later => {
                let (sender, receiver) = oneshot::channel();
                lock(&self.shared.hub)
                    .answers
                    .insert(request_key(&request_id), sender);
                Some(receiver)
            }
            None => None,
        };
        let deferred = self.settle(server, batch.deferred)?;
        if let Some(moved) = moved.filter(|moved| moved != session_id)
            && let Some(connection) = self.connections.remove(session_id)
        {
            self.connections.insert(moved, connection);
        }
        Ok(Dispatched {
            request_id,
            outbound: batch.outbound,
            later,
            deferred,
        })
    }

    /// Closes the connection of `session_id`, once the session is closed.
    pub(super) fn forget(&mut self, session_id: &str) {
        self.connections.remove(session_id);
        lock(&self.shared.hub).sessions.remove(session_id);
    }

    /// The history the mirror of `session_id` holds.
    pub(super) fn history(&self, session_id: &str) -> Result<Vec<PublicHistoryEntry>, ClientError> {
        let hub = lock(&self.shared.hub);
        let mirror = hub.sessions.get(session_id).ok_or_else(|| {
            ClientError::Server(ServerError::SessionNotFound(session_id.to_owned()))
        })?;
        Ok(mirror
            .history()
            .iter()
            .filter_map(|entry| serde_json::from_value(entry.clone()).ok())
            .collect())
    }

    /// The accounting `session_id` last published, if it published any.
    pub(super) fn accounting(&self, session_id: &str) -> Option<(AgentStatsSnapshot, u64)> {
        lock(&self.shared.hub)
            .sessions
            .get(session_id)
            .and_then(|mirror| mirror.accounting.clone())
    }

    /// One field of the state `session_id` last published, without copying
    /// the rest of it.
    pub(super) fn published(&self, session_id: &str, field: PublishedField) -> Option<Value> {
        lock(&self.shared.hub)
            .sessions
            .get(session_id)
            .and_then(|mirror| mirror.state.get(field.key()).cloned())
    }

    /// Opens the channel a turn's entries go to, before the turn can publish
    /// any, and parks its receiving end for the runner.
    pub(super) fn listen(&self, turn_id: &str) {
        let (sender, receiver) = tokio::sync::mpsc::channel(MAX_PROGRAMMATIC_UPDATES);
        let mut hub = lock(&self.shared.hub);
        if hub.parked.contains_key(turn_id) {
            return;
        }
        for update in hub.orphans.remove(turn_id).unwrap_or_default() {
            let _ = sender.try_send(update);
        }
        if !hub.settled.contains_key(turn_id) {
            hub.listeners.insert(turn_id.to_owned(), sender);
        }
        hub.parked.insert(turn_id.to_owned(), receiver);
    }

    /// The receiving end [`Self::listen`] parked for `turn_id`.
    pub(super) fn updates(
        &self,
        turn_id: &str,
    ) -> Result<tokio::sync::mpsc::Receiver<ProgrammaticUpdate>, ClientError> {
        lock(&self.shared.hub)
            .parked
            .remove(turn_id)
            .ok_or_else(|| {
                ClientError::InvalidResponse(format!("turn {turn_id} is not listened to"))
            })
    }

    /// The callbacks sessions asked for since the last drain, oldest first.
    pub(super) fn drain_callbacks(&self) -> Vec<PublicHistoryEntry> {
        lock(&self.shared.hub)
            .callbacks
            .drain(..)
            .filter_map(|callback| serde_json::from_value(callback).ok())
            .collect()
    }

    /// A session this link serves, for a read that names none.
    pub(super) fn any_session(&self) -> Option<String> {
        self.connections.keys().next().cloned()
    }

    /// Reference `AppServerSession.close`: the session closes and its
    /// connection with it.
    pub(super) async fn close(&mut self, server: &AppServer, session_id: &str) {
        if let Ok(dispatched) = self.dispatch(
            server,
            session_id,
            "session/close",
            json!({"sessionId": session_id}),
        ) {
            let _ = PendingPublicCall::unified(server, "session/close", dispatched)
                .complete()
                .await;
        }
        self.forget(session_id);
    }

    /// The session a callback this link delivered belongs to.
    pub(super) fn callback_session(&self, callback_id: &str) -> Option<String> {
        lock(&self.shared.hub)
            .callback_sessions
            .get(callback_id)
            .cloned()
    }
}

/// One request dispatched on a unified session's connection.
pub(super) struct Dispatched {
    pub(super) request_id: RequestId,
    /// The frames the dispatch produced, its answer among them unless the
    /// session gives it later.
    pub(super) outbound: Vec<Vec<u8>>,
    /// Where the answer arrives when the session gives it once the work it
    /// deferred ends.
    pub(super) later: Option<oneshot::Receiver<Vec<u8>>>,
    /// What the dispatch deferred beyond the unified work already started.
    pub(super) deferred: Vec<DeferredWork>,
}

impl Dispatched {
    /// The answer, awaited when the session gives it later.
    pub(super) async fn result(self) -> Result<BTreeMap<String, Value>, ClientError> {
        let frame = match self.later {
            Some(receiver) => receiver.await.map_err(|_| {
                ClientError::InvalidResponse("the session closed before it answered".to_owned())
            })?,
            None => response_frame(self.outbound)?,
        };
        response_result(frame, &self.request_id)
    }

    /// The answer, which has to be there already.
    pub(super) fn now(self) -> Result<BTreeMap<String, Value>, ClientError> {
        if self.later.is_some() {
            return Err(ClientError::InvalidResponse(
                "the request is answered once its work ends".to_owned(),
            ));
        }
        response_result(response_frame(self.outbound)?, &self.request_id)
    }
}

/// Waits for `turn_id` to settle and answers what it published, as a turn a
/// driver ran would have.
pub(super) async fn settled_turn(
    shared: Arc<Shared>,
    session_id: String,
    turn_id: String,
) -> Result<TurnOutcome, DriverError> {
    let turn = loop {
        let notified = shared.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let mut hub = lock(&shared.hub);
            if let Some(turn) = hub.settled.remove(&turn_id) {
                hub.parked.remove(&turn_id);
                break turn;
            }
        }
        notified.await;
    };
    let hub = lock(&shared.hub);
    let mirror = hub.sessions.get(&session_id);
    let history: Vec<PublicHistoryEntry> = mirror
        .map(|mirror| {
            mirror
                .history()
                .iter()
                .filter_map(|entry| serde_json::from_value(entry.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    let session = mirror
        .map(|mirror| mirror.state["session"].clone())
        .unwrap_or(Value::Null);
    drop(hub);
    let status = turn["status"].as_str().unwrap_or_default();
    if status == "failed" {
        let error =
            serde_json::from_value::<PublicError>(turn["error"].clone()).unwrap_or(PublicError {
                message: "The turn failed".to_owned(),
                code: None,
                details: Value::Null,
            });
        return Err(DriverError::Published(error));
    }
    let stop_reason = match (status, turn["stopReason"].as_str()) {
        ("cancelled", _) => TurnStopReason::Cancelled,
        (_, Some("limit")) => TurnStopReason::MaxSteps,
        _ => TurnStopReason::Complete,
    };
    let usage = &session["tokenUsage"];
    let context = &session["contextUsage"];
    Ok(TurnOutcome {
        session_id: session_id.clone(),
        events: Vec::new(),
        snapshot: vibe_core::events::ProjectionSnapshot {
            session_id,
            turn_id: Some(turn_id),
            handoff_cause: None,
            watermark: 0,
            lifecycle: LifecycleState::Idle,
            title: session["title"].as_str().map(ToOwned::to_owned),
            history,
        },
        messages: Vec::new(),
        usage: Usage {
            input_tokens: usage["inputTokens"].as_u64().unwrap_or(0),
            output_tokens: usage["outputTokens"].as_u64().unwrap_or(0),
            cached_tokens: 0,
        },
        context_tokens: context["totalTokens"].as_u64().unwrap_or(0),
        price_micros: 0,
        steps: 0,
        checkpoints: 0,
        stop_reason,
    })
}

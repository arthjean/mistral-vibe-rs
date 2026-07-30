use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::hint::black_box;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use vibe_core::engine::{
    CancellationToken, CompletionProvider, ConversationEngine, EventObserver, ProviderFuture,
    ProviderStreamFuture,
};
use vibe_core::events::{
    ApplyOutcome, EngineEvent, EventEnvelope, ModelMessage, ProjectionReducer, ProjectionSnapshot,
    PublicHistoryEntry,
};
use vibe_core::provider::{
    ProviderChunk, ProviderError, ProviderInput, ProviderStream, RequestLimits,
};
use vibe_protocol::{Envelope, ProtocolValidationError, decode_frame, encode_frame};

use crate::client::{DriverError, TurnDriver, TurnReservation, public_turn_error};
use crate::server::{AppServer, DeferredWork, ServerError};

pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const TASK_CLEANUP_GRACE: Duration = Duration::from_secs(5);

pub struct MemoryTransport {
    incoming: mpsc::Receiver<Vec<u8>>,
    outgoing: mpsc::Sender<Vec<u8>>,
}

impl MemoryTransport {
    #[must_use]
    pub fn pair(capacity: usize) -> (Self, Self) {
        let (left_tx, left_rx) = mpsc::channel(capacity);
        let (right_tx, right_rx) = mpsc::channel(capacity);
        (
            Self {
                incoming: left_rx,
                outgoing: right_tx,
            },
            Self {
                incoming: right_rx,
                outgoing: left_tx,
            },
        )
    }

    pub async fn send(&self, frame: &Envelope) -> Result<(), TransportError> {
        let bytes = encode_frame(frame)?;
        self.outgoing
            .send(bytes)
            .await
            .map_err(|_| TransportError::Closed)
    }

    pub async fn receive(&mut self) -> Result<Option<Envelope>, TransportError> {
        self.incoming
            .recv()
            .await
            .map(|bytes| decode_frame(&bytes).map_err(TransportError::Protocol))
            .transpose()
    }

    pub async fn send_raw(&self, bytes: Vec<u8>) -> Result<(), TransportError> {
        self.outgoing
            .send(bytes)
            .await
            .map_err(|_| TransportError::Closed)
    }

    pub async fn receive_raw(&mut self) -> Option<Vec<u8>> {
        self.incoming.recv().await
    }
}

pub struct StdioTransport<R, W> {
    reader: R,
    writer: W,
}

impl<R, W> StdioTransport<R, W>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    #[must_use]
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }

    pub async fn receive(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        read_bounded_frame(&mut self.reader).await
    }

    pub async fn send(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        self.writer
            .write_all(bytes)
            .await
            .map_err(TransportError::Io)?;
        self.writer
            .write_all(b"\n")
            .await
            .map_err(TransportError::Io)?;
        self.writer.flush().await.map_err(TransportError::Io)
    }

    pub async fn close(&mut self) -> Result<(), TransportError> {
        self.writer.shutdown().await.map_err(TransportError::Io)
    }
}

pub async fn read_bounded_frame<R>(reader: &mut R) -> Result<Option<Vec<u8>>, TransportError>
where
    R: AsyncBufRead + Unpin,
{
    let mut frame = Vec::new();
    loop {
        let (consumed, found_newline, reached_eof) = {
            let available = reader.fill_buf().await.map_err(TransportError::Io)?;
            if available.is_empty() {
                (0, false, true)
            } else {
                let newline = available.iter().position(|byte| *byte == b'\n');
                let consumed = newline.map_or(available.len(), |index| index.saturating_add(1));
                if frame.len().saturating_add(consumed) > MAX_FRAME_BYTES {
                    return Err(TransportError::FrameTooLarge {
                        limit: MAX_FRAME_BYTES,
                    });
                }
                frame.extend_from_slice(&available[..consumed]);
                (consumed, newline.is_some(), false)
            }
        };
        if reached_eof {
            if frame.is_empty() {
                return Ok(None);
            }
            break;
        }
        reader.consume(consumed);
        if found_newline {
            break;
        }
    }
    while matches!(frame.last(), Some(b'\n' | b'\r')) {
        frame.pop();
    }
    if frame.is_empty() {
        return Err(TransportError::EmptyFrame);
    }
    Ok(Some(frame))
}

pub async fn serve_stdio<R, W, D>(
    server: AppServer,
    mut transport: StdioTransport<R, W>,
    driver: Arc<D>,
) -> Result<(), TransportError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
    D: TurnDriver + 'static,
{
    let mut connection = server.connect(vibe_protocol::TransportKind::Stdio);
    let (completed_tx, mut completed_rx) = mpsc::unbounded_channel::<DeferredTaskEvent>();
    let mut tasks = JoinSet::new();
    let mut active = BTreeSet::<(String, String)>::new();
    let mut failure = None;
    'serve: loop {
        tokio::select! {
            event = completed_rx.recv(), if !active.is_empty() => {
                let Some(event) = event else {
                    break;
                };
                let bytes = match event {
                    DeferredTaskEvent::Notification(bytes) => bytes,
                    DeferredTaskEvent::Completion(completion) => {
                        active.remove(&(completion.session_id, completion.turn_id));
                        match completion.notification {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                failure = Some(TransportError::Server(error));
                                break 'serve;
                            }
                        }
                    }
                };
                if let Err(error) = transport.send(&bytes).await {
                    failure = Some(error);
                    break 'serve;
                }
                while tasks.try_join_next().is_some() {}
            }
            incoming = transport.receive() => {
                let bytes = match incoming {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => break,
                    Err(error) => {
                        failure = Some(error);
                        break 'serve;
                    }
                };
                let batch = connection.dispatch(&bytes);
                for outbound in batch.outbound {
                    if let Err(error) = transport.send(&outbound).await {
                        fail_deferred(&server, &batch.deferred, "transport response write failed")
                            .await;
                        failure = Some(error);
                        break 'serve;
                    }
                }
                let close_after_work = batch.close_after_flush;
                for work in batch.deferred {
                    match work {
                        DeferredWork::RunTurn {
                            session_id,
                            turn_id,
                            prompt,
                            input,
                            client_user_message_id,
                            auto_title,
                            user_display_content,
                            mention_stats,
                        } => {
                            let session = match server.session(&session_id) {
                                Ok(session) => session,
                                Err(error) => {
                                    failure = Some(TransportError::Server(error));
                                    break 'serve;
                                }
                            };
                            let tools = match server.tool_registry(&session_id) {
                                Ok(tools) => tools,
                                Err(error) => {
                                    failure = Some(TransportError::Server(error));
                                    break 'serve;
                                }
                            };
                            let reservation = TurnReservation {
                                session_id: session_id.clone(),
                                turn_id: turn_id.clone(),
                                prompt,
                                input,
                                client_user_message_id,
                                auto_title,
                                user_display_content,
                                mention_stats,
                                working_directory: session.working_directory,
                                intent: session.intent,
                                tools,
                            };
                            active.insert((session_id.clone(), turn_id.clone()));
                            let task_server = server.clone();
                            let task_driver = Arc::clone(&driver);
                            let task_tx = completed_tx.clone();
                            tasks.spawn(async move {
                                match task_server.turn_started(
                                    &reservation.session_id,
                                    &reservation.turn_id,
                                ) {
                                    Ok(bytes) => {
                                        let _ = task_tx
                                            .send(DeferredTaskEvent::Notification(bytes));
                                    }
                                    Err(error) => {
                                        let _ = task_tx.send(DeferredTaskEvent::Completion(
                                            DeferredCompletion {
                                                session_id,
                                                turn_id,
                                                notification: Err(error),
                                            },
                                        ));
                                        return;
                                    }
                                }
                                let (observer, mut updates) =
                                    app_server_update_channel_for_turn(
                                        reservation.session_id.clone(),
                                        reservation.turn_id.clone(),
                                    );
                                let mut turn = Box::pin(
                                    task_driver.run_observed(&reservation, observer),
                                );
                                let outcome = loop {
                                    tokio::select! {
                                        outcome = &mut turn => break outcome,
                                        update = updates.recv() => {
                                            if let Some(update) = update {
                                                match app_server_notification(&task_server, update) {
                                                    Ok(bytes) => {
                                                        let _ = task_tx.send(
                                                            DeferredTaskEvent::Notification(bytes),
                                                        );
                                                    }
                                                    Err(error) => {
                                                        let _ = task_tx.send(
                                                            DeferredTaskEvent::Completion(
                                                                DeferredCompletion {
                                                                    session_id,
                                                                    turn_id,
                                                                    notification: Err(error),
                                                                },
                                                            ),
                                                        );
                                                        return;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                };
                                while let Ok(update) = updates.try_recv() {
                                    match app_server_notification(&task_server, update) {
                                        Ok(bytes) => {
                                            let _ = task_tx
                                                .send(DeferredTaskEvent::Notification(bytes));
                                        }
                                        Err(error) => {
                                            let _ = task_tx.send(
                                                DeferredTaskEvent::Completion(DeferredCompletion {
                                                    session_id,
                                                    turn_id,
                                                    notification: Err(error),
                                                }),
                                            );
                                            return;
                                        }
                                    }
                                }
                                let notification = match outcome {
                                    Ok(outcome) => {
                                        let stop_reason = matches!(
                                            outcome.stop_reason,
                                            crate::client::PublicTurnStopReason::MaxSteps
                                                | crate::client::PublicTurnStopReason::TokenLimit
                                                | crate::client::PublicTurnStopReason::PriceLimit
                                        )
                                        .then_some(vibe_core::events::PublicTurnStopReason::Limit);
                                        let error = public_turn_error(&outcome.stop_reason);
                                        task_server.complete_turn_with_details(
                                            &reservation.session_id,
                                            &reservation.turn_id,
                                            outcome.snapshot,
                                            stop_reason,
                                            error,
                                        )
                                    }
                                    Err(error) => task_server.fail_turn(
                                        &reservation.session_id,
                                        &reservation.turn_id,
                                        &error.to_string(),
                                    ),
                                };
                                let _ = task_tx.send(DeferredTaskEvent::Completion(
                                    DeferredCompletion {
                                        session_id,
                                        turn_id,
                                        notification,
                                    },
                                ));
                            });
                        }
                        DeferredWork::InterruptTurn {
                            session_id,
                            turn_id,
                        } => {
                            if let Err(error) = driver.interrupt(&session_id, &turn_id) {
                                let notification = server
                                    .fail_turn(&session_id, &turn_id, &error.to_string())
                                    .map_err(TransportError::Server);
                                match notification {
                                    Ok(bytes) => {
                                        if let Err(error) = transport.send(&bytes).await {
                                            failure = Some(error);
                                            break 'serve;
                                        }
                                    }
                                    Err(error) => {
                                        failure = Some(error);
                                        break 'serve;
                                    }
                                }
                            }
                        }
                        DeferredWork::SteerTurn {
                            session_id,
                            turn_id,
                            content,
                        } => {
                            if let Err(error) = driver.steer(&session_id, &turn_id, &content) {
                                failure = Some(TransportError::Driver(error));
                                break 'serve;
                            }
                        }
                        DeferredWork::InjectContext {
                            session_id,
                            content,
                            as_message,
                        } => {
                            if let Err(error) = driver.inject_context(
                                &session_id,
                                &content,
                                as_message,
                            ) {
                                failure = Some(TransportError::Driver(error));
                                break 'serve;
                            }
                        }
                        DeferredWork::ResolveCallback {
                            session_id,
                            turn_id,
                            callback_id,
                            accepted,
                            value,
                        } => {
                            if let Err(error) = driver.resolve_callback(
                                &session_id,
                                &turn_id,
                                &callback_id,
                                accepted,
                                value.as_deref(),
                            ) {
                                failure = Some(TransportError::Driver(error));
                                break 'serve;
                            }
                        }
                        DeferredWork::ResourceRequest {
                            request_id,
                            session_id,
                            method,
                            params,
                        } => {
                            let response = server
                                .execute_resource_request(
                                    request_id,
                                    session_id,
                                    method,
                                    params,
                                )
                                .await;
                            for outbound in response.outbound {
                                if let Err(error) = transport.send(&outbound).await {
                                    failure = Some(error);
                                    break 'serve;
                                }
                            }
                        }
                        DeferredWork::CloudRequest {
                            request_id,
                            method,
                            params,
                        } => {
                            let response = server
                                .execute_cloud_request(request_id, method, params)
                                .await;
                            for outbound in response.outbound {
                                if let Err(error) = transport.send(&outbound).await {
                                    failure = Some(error);
                                    break 'serve;
                                }
                            }
                        }
                        DeferredWork::ConfigureMcp {
                            session_id,
                            configs,
                        } => {
                            let notification = match server
                                .configure_mcp_servers(&session_id, configs)
                                .await
                            {
                                Ok(notification) => notification,
                                Err(error) => {
                                    failure = Some(TransportError::Server(error));
                                    break 'serve;
                                }
                            };
                            if let Err(error) = transport.send(&notification).await {
                                failure = Some(error);
                                break 'serve;
                            }
                        }
                        DeferredWork::CompactSession {
                            request_id,
                            session_id,
                            extra_instructions,
                        } => {
                            let batch = match driver
                                .compact(&session_id, &extra_instructions)
                                .await
                            {
                                Ok(compaction) => server.complete_manual_compaction(
                                    request_id,
                                    &session_id,
                                    &compaction.new_session_id,
                                    &compaction.summary,
                                    compaction.hydrated,
                                ),
                                Err(error) => server.fail_manual_compaction(
                                    request_id,
                                    &session_id,
                                    &error.to_string(),
                                ),
                            };
                            for outbound in batch.outbound {
                                if let Err(error) = transport.send(&outbound).await {
                                    failure = Some(error);
                                    break 'serve;
                                }
                            }
                        }
                        DeferredWork::CloseResources {
                            session_id,
                            generation,
                        } => {
                            if let Err(error) =
                                server.close_resource_session(&session_id, generation).await
                            {
                                failure = Some(TransportError::Server(error));
                                break 'serve;
                            }
                        }
                    }
                }
                if close_after_work {
                    break;
                }
            }
        }
    }
    let detached_sessions = connection.attached_session_ids();
    connection.close();
    for (session_id, turn_id) in active {
        let _ = driver.interrupt(&session_id, &turn_id);
    }
    let drained = tokio::time::timeout(TASK_CLEANUP_GRACE, async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_ok();
    if !drained {
        tasks.shutdown().await;
    }
    for session_id in detached_sessions {
        let orphaned_generation = server
            .orphaned_resource_generation(&session_id)
            .unwrap_or(None);
        if let Some(generation) = orphaned_generation
            && let Err(error) = server.close_resource_session(&session_id, generation).await
            && failure.is_none()
        {
            failure = Some(TransportError::Server(error));
        }
    }
    let close_result = transport.close().await;
    match failure {
        Some(error) => Err(error),
        None => close_result,
    }
}

struct DeferredCompletion {
    session_id: String,
    turn_id: String,
    notification: Result<Vec<u8>, ServerError>,
}

enum DeferredTaskEvent {
    Notification(Vec<u8>),
    Completion(DeferredCompletion),
}

#[derive(Debug)]
enum AppServerUpdate {
    HistoryAdded {
        session_id: String,
        turn_id: String,
        emitted_at: u64,
        entry: Box<PublicHistoryEntry>,
        snapshot: ProjectionSnapshot,
    },
    HistoryUpdated {
        session_id: String,
        turn_id: String,
        emitted_at: u64,
        entry_id: String,
        patch: Vec<JsonPatchOperation>,
        snapshot: ProjectionSnapshot,
    },
    SessionCompacted {
        old_session_id: String,
        new_session_id: String,
        turn_id: String,
        emitted_at: u64,
        snapshot: ProjectionSnapshot,
        summary_length: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct JsonPatchOperation {
    op: &'static str,
    path: String,
    value: Value,
}

struct AppServerProjection {
    reducer: ProjectionReducer,
    entries: BTreeMap<String, PublicHistoryEntry>,
    summary_length: usize,
}

struct AppServerEventObserver {
    projection: Mutex<AppServerProjection>,
    sender: mpsc::UnboundedSender<AppServerUpdate>,
}

fn app_server_update_channel_for_turn(
    session_id: impl Into<String>,
    turn_id: impl Into<String>,
) -> (
    Arc<dyn EventObserver>,
    mpsc::UnboundedReceiver<AppServerUpdate>,
) {
    let (sender, receiver) = mpsc::unbounded_channel();
    (
        Arc::new(AppServerEventObserver {
            projection: Mutex::new(AppServerProjection {
                reducer: ProjectionReducer::for_turn(session_id, turn_id),
                entries: BTreeMap::new(),
                summary_length: 0,
            }),
            sender,
        }),
        receiver,
    )
}

impl EventObserver for AppServerEventObserver {
    fn observe(&self, event: &EventEnvelope) -> Result<(), String> {
        let mut projection = self
            .projection
            .lock()
            .map_err(|_| "app-server projection lock is poisoned".to_owned())?;
        if projection
            .reducer
            .apply(event)
            .map_err(|error| error.to_string())?
            == ApplyOutcome::Duplicate
        {
            return Ok(());
        }

        if let EngineEvent::Compaction { summary } = &event.event {
            projection.summary_length = summary.chars().count();
        }
        if let EngineEvent::SessionHandoff {
            from_session_id,
            to_session_id,
        } = &event.event
        {
            let snapshot = projection.reducer.state().clone();
            let turn_id = snapshot
                .turn_id
                .clone()
                .ok_or_else(|| "session handoff has no active turn".to_owned())?;
            projection.entries = snapshot
                .history
                .iter()
                .map(|entry| (entry.metadata().id.clone(), entry.clone()))
                .collect();
            self.sender
                .send(AppServerUpdate::SessionCompacted {
                    old_session_id: from_session_id.clone(),
                    new_session_id: to_session_id.clone(),
                    turn_id,
                    emitted_at: event.emitted_at,
                    snapshot,
                    summary_length: projection.summary_length,
                })
                .map_err(|_| "app-server update receiver is closed".to_owned())?;
            return Ok(());
        }

        let snapshot = projection.reducer.state().clone();
        let turn_id = snapshot
            .turn_id
            .clone()
            .ok_or_else(|| "live history update has no active turn".to_owned())?;
        for entry in &snapshot.history {
            let entry_id = entry.metadata().id.clone();
            match projection.entries.get(&entry_id) {
                None => self
                    .sender
                    .send(AppServerUpdate::HistoryAdded {
                        session_id: snapshot.session_id.clone(),
                        turn_id: turn_id.clone(),
                        emitted_at: event.emitted_at,
                        entry: Box::new(entry.clone()),
                        snapshot: snapshot.clone(),
                    })
                    .map_err(|_| "app-server update receiver is closed".to_owned())?,
                Some(previous) if previous != entry => {
                    let patch = history_entry_patch(previous, entry)?;
                    self.sender
                        .send(AppServerUpdate::HistoryUpdated {
                            session_id: snapshot.session_id.clone(),
                            turn_id: turn_id.clone(),
                            emitted_at: event.emitted_at,
                            entry_id,
                            patch,
                            snapshot: snapshot.clone(),
                        })
                        .map_err(|_| "app-server update receiver is closed".to_owned())?;
                }
                Some(_) => {}
            }
        }
        projection.entries = snapshot
            .history
            .into_iter()
            .map(|entry| (entry.metadata().id.clone(), entry))
            .collect();
        Ok(())
    }
}

fn history_entry_patch(
    previous: &PublicHistoryEntry,
    current: &PublicHistoryEntry,
) -> Result<Vec<JsonPatchOperation>, String> {
    let previous = serde_json::to_value(previous).map_err(|error| error.to_string())?;
    let current = serde_json::to_value(current).map_err(|error| error.to_string())?;
    let mut patch = Vec::new();
    diff_json("", &previous, &current, &mut patch);
    Ok(patch)
}

fn diff_json(path: &str, previous: &Value, current: &Value, patch: &mut Vec<JsonPatchOperation>) {
    if previous == current {
        return;
    }
    match (previous, current) {
        (Value::Object(previous), Value::Object(current)) => {
            let keys = previous
                .keys()
                .chain(current.keys())
                .cloned()
                .collect::<BTreeSet<_>>();
            for key in keys {
                let child_path = format!("{path}/{}", escape_json_pointer(&key));
                match (previous.get(&key), current.get(&key)) {
                    (Some(previous), Some(current)) => {
                        diff_json(&child_path, previous, current, patch);
                    }
                    (None, Some(value)) => patch.push(JsonPatchOperation {
                        op: "add",
                        path: child_path,
                        value: value.clone(),
                    }),
                    (Some(_), None) => patch.push(JsonPatchOperation {
                        op: "remove",
                        path: child_path,
                        value: Value::Null,
                    }),
                    (None, None) => {}
                }
            }
        }
        (Value::Array(previous), Value::Array(current)) if previous.len() == current.len() => {
            for (index, (previous, current)) in previous.iter().zip(current).enumerate() {
                diff_json(&format!("{path}/{index}"), previous, current, patch);
            }
        }
        (Value::String(previous), Value::String(current))
            if is_append_path(path) && current.starts_with(previous) =>
        {
            patch.push(JsonPatchOperation {
                op: "append",
                path: path.to_owned(),
                value: Value::String(current[previous.len()..].to_owned()),
            });
        }
        _ => patch.push(JsonPatchOperation {
            op: "replace",
            path: path.to_owned(),
            value: current.clone(),
        }),
    }
}

fn escape_json_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

fn is_append_path(path: &str) -> bool {
    path == "/state/outputText" || path.ends_with("/text")
}

fn app_server_notification(
    server: &AppServer,
    update: AppServerUpdate,
) -> Result<Vec<u8>, ServerError> {
    let value = match update {
        AppServerUpdate::HistoryAdded {
            session_id,
            turn_id,
            emitted_at,
            entry,
            snapshot,
        } => {
            let event_id = server.apply_live_projection(&session_id, &turn_id, snapshot)?;
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "history/entryAdded",
                "params": {
                    "eventId": event_id,
                    "sessionId": session_id,
                    "turnId": turn_id,
                    "entry": entry,
                    "emittedAt": emitted_at,
                }
            })
        }
        AppServerUpdate::HistoryUpdated {
            session_id,
            turn_id,
            emitted_at,
            entry_id,
            patch,
            snapshot,
        } => {
            let event_id = server.apply_live_projection(&session_id, &turn_id, snapshot)?;
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "history/entryUpdated",
                "params": {
                    "eventId": event_id,
                    "sessionId": session_id,
                    "turnId": turn_id,
                    "entryId": entry_id,
                    "patch": patch,
                    "emittedAt": emitted_at,
                }
            })
        }
        AppServerUpdate::SessionCompacted {
            old_session_id,
            new_session_id,
            turn_id,
            emitted_at,
            snapshot,
            summary_length,
        } => {
            return server.handoff_active_turn(
                &old_session_id,
                &new_session_id,
                &turn_id,
                snapshot,
                summary_length,
                emitted_at,
            );
        }
    };
    serde_json::to_vec(&value).map_err(ServerError::Json)
}

const STREAMING_P95_TARGET_MICROS: u64 = 20_000;
const STREAMING_P99_TARGET_MICROS: u64 = 50_000;
const RELEASE_STREAMING_SAMPLE_SIZE: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamingLatencyBenchmark {
    pub chunk_count: usize,
    pub client_visible_event_count: usize,
    pub p50_micros: u64,
    pub p95_micros: u64,
    pub p99_micros: u64,
    pub max_micros: u64,
    pub total_elapsed_millis: u64,
    pub chunks_per_second: u64,
    pub p95_target_micros: u64,
    pub p99_target_micros: u64,
    pub release_sample_size: usize,
    pub release_gate_passed: bool,
}

struct BenchmarkProvider {
    chunk_count: usize,
    receipts: Arc<Mutex<VecDeque<Instant>>>,
}

impl CompletionProvider for BenchmarkProvider {
    fn complete<'a>(&'a self, _input: &'a ProviderInput) -> ProviderFuture<'a> {
        Box::pin(async {
            Err(ProviderError::MalformedStream(
                "streaming benchmark requires the streaming path".to_owned(),
            ))
        })
    }

    fn stream<'a>(&'a self, _input: &'a ProviderInput) -> ProviderStreamFuture<'a> {
        let chunk_count = self.chunk_count;
        let receipts = Arc::clone(&self.receipts);
        Box::pin(async move {
            let text = futures_util::stream::iter(0..chunk_count).map(move |_| {
                if let Ok(mut receipts) = receipts.lock() {
                    receipts.push_back(Instant::now());
                }
                Ok(ProviderChunk::Text {
                    text: "x".to_owned(),
                })
            });
            let terminal = futures_util::stream::iter([
                Ok(ProviderChunk::Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                }),
                Ok(ProviderChunk::Stop {
                    reason: "stop".to_owned(),
                }),
            ]);
            Ok(ProviderStream {
                correlation_id: Some("fake-streaming-benchmark".to_owned()),
                chunks: Box::pin(text.chain(terminal)),
            })
        })
    }
}

struct BenchmarkObserver {
    projection: Arc<dyn EventObserver>,
    updates: Mutex<mpsc::UnboundedReceiver<AppServerUpdate>>,
    server: AppServer,
    receipts: Arc<Mutex<VecDeque<Instant>>>,
    latencies: Arc<Mutex<Vec<Duration>>>,
}

impl EventObserver for BenchmarkObserver {
    fn observe(&self, event: &EventEnvelope) -> Result<(), String> {
        let receipt = if matches!(&event.event, EngineEvent::ModelText { .. }) {
            Some(
                self.receipts
                    .lock()
                    .map_err(|_| "benchmark receipt lock is poisoned".to_owned())?
                    .pop_front()
                    .ok_or_else(|| "benchmark model event has no receipt timestamp".to_owned())?,
            )
        } else {
            None
        };
        self.projection.observe(event)?;
        let mut updates = self
            .updates
            .lock()
            .map_err(|_| "benchmark update lock is poisoned".to_owned())?;
        let mut visible_updates = 0_usize;
        while let Ok(update) = updates.try_recv() {
            let bytes =
                app_server_notification(&self.server, update).map_err(|error| error.to_string())?;
            black_box(bytes);
            visible_updates = visible_updates.saturating_add(1);
        }
        if let Some(receipt) = receipt {
            if visible_updates == 0 {
                return Err("benchmark model chunk produced no client-visible event".to_owned());
            }
            self.latencies
                .lock()
                .map_err(|_| "benchmark latency lock is poisoned".to_owned())?
                .push(receipt.elapsed());
        }
        Ok(())
    }
}

pub async fn benchmark_fake_provider_chunk_latency(
    chunk_count: usize,
) -> Result<StreamingLatencyBenchmark, String> {
    if chunk_count == 0 {
        return Err("streaming benchmark requires at least one chunk".to_owned());
    }

    let server = AppServer::default();
    let mut connection = server.connect(vibe_protocol::TransportKind::InProcess);
    for frame in [
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "streaming-benchmark",
                    "version": env!("CARGO_PKG_VERSION"),
                    "entrypoint": "programmatic",
                    "terminalEmulator": "unknown"
                },
                "capabilities": {}
            }
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/start",
            "params": {
                "sessionId": "streaming-benchmark-session",
                "workingDirectory": "/streaming-benchmark"
            }
        }),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "turn/start",
            "params": {
                "sessionId": "streaming-benchmark-session",
                "input": [{"type": "text", "text": "benchmark"}]
            }
        }),
    ] {
        let batch = connection.dispatch(
            &serde_json::to_vec(&frame)
                .map_err(|error| format!("benchmark request serialization failed: {error}"))?,
        );
        if batch.close_after_flush {
            return Err("benchmark app-server connection closed during setup".to_owned());
        }
    }
    let session = server
        .session("streaming-benchmark-session")
        .map_err(|error| error.to_string())?;
    let turn_id = session
        .active_turn
        .ok_or_else(|| "benchmark turn was not reserved".to_owned())?;
    black_box(
        server
            .turn_started("streaming-benchmark-session", &turn_id)
            .map_err(|error| error.to_string())?,
    );

    let receipts = Arc::new(Mutex::new(VecDeque::with_capacity(chunk_count)));
    let latencies = Arc::new(Mutex::new(Vec::with_capacity(chunk_count)));
    let (projection, updates) =
        app_server_update_channel_for_turn("streaming-benchmark-session", turn_id.clone());
    let observer: Arc<dyn EventObserver> = Arc::new(BenchmarkObserver {
        projection,
        updates: Mutex::new(updates),
        server,
        receipts: Arc::clone(&receipts),
        latencies: Arc::clone(&latencies),
    });
    let provider = BenchmarkProvider {
        chunk_count,
        receipts,
    };
    let input = ProviderInput {
        turn_id: Some(turn_id),
        messages: vec![ModelMessage::System {
            content: "deterministic streaming benchmark".to_owned(),
        }],
        stream: true,
        images: Vec::new(),
        tools: Vec::new(),
        tool_choice: None,
        thinking: false,
        reasoning_effort: None,
        headers: BTreeMap::new(),
        limits: RequestLimits {
            max_tokens: u32::MAX,
            temperature_millis: None,
            max_response_bytes: 2 * 1024 * 1024,
        },
        metadata: BTreeMap::new(),
    };
    let total_started = Instant::now();
    ConversationEngine::new(provider)
        .with_observer(observer)
        .run_turn(
            "streaming-benchmark-session",
            input,
            "benchmark",
            CancellationToken::default(),
        )
        .await
        .map_err(|error| error.to_string())?;
    let total_elapsed = total_started.elapsed();

    let mut latency_micros = latencies
        .lock()
        .map_err(|_| "benchmark latency lock is poisoned".to_owned())?
        .iter()
        .copied()
        .map(duration_micros)
        .collect::<Vec<_>>();
    if latency_micros.len() != chunk_count {
        return Err(format!(
            "benchmark observed {} client-visible chunk events, expected {chunk_count}",
            latency_micros.len()
        ));
    }
    latency_micros.sort_unstable();
    let p50_micros = percentile(&latency_micros, 50);
    let p95_micros = percentile(&latency_micros, 95);
    let p99_micros = percentile(&latency_micros, 99);
    let max_micros = latency_micros.last().copied().unwrap_or_default();
    let total_elapsed_millis = u64::try_from(total_elapsed.as_millis()).unwrap_or(u64::MAX);
    let total_nanos = total_elapsed.as_nanos().max(1);
    let chunks_per_second = u64::try_from(
        (chunk_count as u128)
            .saturating_mul(1_000_000_000)
            .saturating_div(total_nanos),
    )
    .unwrap_or(u64::MAX);
    let release_gate_passed = chunk_count >= RELEASE_STREAMING_SAMPLE_SIZE
        && p95_micros <= STREAMING_P95_TARGET_MICROS
        && p99_micros <= STREAMING_P99_TARGET_MICROS;
    Ok(StreamingLatencyBenchmark {
        chunk_count,
        client_visible_event_count: latency_micros.len(),
        p50_micros,
        p95_micros,
        p99_micros,
        max_micros,
        total_elapsed_millis,
        chunks_per_second,
        p95_target_micros: STREAMING_P95_TARGET_MICROS,
        p99_target_micros: STREAMING_P99_TARGET_MICROS,
        release_sample_size: RELEASE_STREAMING_SAMPLE_SIZE,
        release_gate_passed,
    })
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn percentile(sorted: &[u64], percentage: usize) -> u64 {
    let rank = sorted
        .len()
        .saturating_mul(percentage)
        .saturating_add(99)
        .saturating_div(100)
        .saturating_sub(1);
    sorted.get(rank).copied().unwrap_or_default()
}

async fn fail_deferred(server: &AppServer, deferred: &[DeferredWork], message: &str) {
    for work in deferred {
        match work {
            DeferredWork::RunTurn {
                session_id,
                turn_id,
                ..
            } => {
                let _ = server.fail_turn(session_id, turn_id, message);
            }
            DeferredWork::CloseResources {
                session_id,
                generation,
            } => {
                let _ = server.close_resource_session(session_id, *generation).await;
            }
            DeferredWork::InterruptTurn { .. }
            | DeferredWork::SteerTurn { .. }
            | DeferredWork::InjectContext { .. }
            | DeferredWork::ResolveCallback { .. }
            | DeferredWork::ResourceRequest { .. }
            | DeferredWork::CloudRequest { .. }
            | DeferredWork::ConfigureMcp { .. } => {}
            DeferredWork::CompactSession {
                request_id,
                session_id,
                ..
            } => {
                let _ = server.fail_manual_compaction(request_id.clone(), session_id, message);
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error(transparent)]
    Protocol(#[from] ProtocolValidationError),
    #[error("transport I/O failed: {0}")]
    Io(io::Error),
    #[error("transport is closed")]
    Closed,
    #[error("empty transport frame")]
    EmptyFrame,
    #[error("transport frame exceeded the {limit}-byte limit")]
    FrameTooLarge { limit: usize },
    #[error("app-server lifecycle failed: {0}")]
    Server(ServerError),
    #[error("turn driver failed: {0}")]
    Driver(DriverError),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio::io::{BufReader, duplex};
    use vibe_protocol::{JsonRpcVersion, Notification};

    use super::*;
    use crate::client::EchoTurnDriver;
    use crate::resources::{
        ResourceBackend, ResourceBackendRequest, ResourceDispatch, ResourceError, ResourceFuture,
        ResourceSession,
    };
    use crate::server::AppServer;

    #[derive(Default)]
    struct CleanupResourceBackend {
        opened: AtomicBool,
        closed: AtomicBool,
    }

    impl ResourceBackend for CleanupResourceBackend {
        fn open_session(&self, _session: ResourceSession) -> Result<(), ResourceError> {
            self.opened.store(true, Ordering::Release);
            Ok(())
        }

        fn dispatch<'a>(
            &'a self,
            request: ResourceBackendRequest,
        ) -> ResourceFuture<'a, ResourceDispatch> {
            Box::pin(async move { Err(ResourceError::MethodNotFound(request.method)) })
        }

        fn close_session<'a>(
            &'a self,
            _session_id: &'a str,
            _generation: u64,
        ) -> ResourceFuture<'a, ()> {
            Box::pin(async move {
                self.closed.store(true, Ordering::Release);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn memory_transport_crosses_a_json_boundary() {
        let (left, mut right) = MemoryTransport::pair(1);
        let frame = Envelope::Notification(Notification {
            jsonrpc: JsonRpcVersion::V2,
            method: "initialized".to_owned(),
            params: BTreeMap::new(),
        });
        left.send(&frame).await.expect("memory send");
        assert_eq!(right.receive().await.expect("memory receive"), Some(frame));
    }

    #[test]
    fn compaction_rebinds_history_and_resets_the_new_session_watermark() {
        let server = AppServer::default();
        let mut connection = server.connect(vibe_protocol::TransportKind::InProcess);
        for frame in [
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "test",
                        "version": "1",
                        "entrypoint": "programmatic",
                        "terminalEmulator": "unknown"
                    },
                    "capabilities": {}
                }
            }),
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "initialized",
                "params": {}
            }),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "session/start",
                "params": {"sessionId": "session-1", "workingDirectory": "/workspace"}
            }),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "turn/start",
                "params": {
                    "sessionId": "session-1",
                    "input": [{"type": "text", "text": "compact"}]
                }
            }),
        ] {
            connection.dispatch(&serde_json::to_vec(&frame).expect("request frame"));
        }
        let started = server
            .turn_started("session-1", "turn-1")
            .expect("turn starts");
        let started = decode_frame(&started).expect("started notification");
        assert!(matches!(
            started,
            Envelope::Notification(Notification { params, .. })
                if params["eventId"] == 1
        ));

        let (observer, mut updates) = app_server_update_channel_for_turn("session-1", "turn-1");
        for event in [
            EventEnvelope {
                session_id: "session-1".to_owned(),
                turn_id: Some("turn-1".to_owned()),
                emitted_at: 10,
                event_id: 1,
                event: EngineEvent::UserMessage {
                    content: "compact".to_owned(),
                },
            },
            EventEnvelope {
                session_id: "session-1".to_owned(),
                turn_id: Some("turn-1".to_owned()),
                emitted_at: 11,
                event_id: 2,
                event: EngineEvent::Compaction {
                    summary: "short".to_owned(),
                },
            },
            EventEnvelope {
                session_id: "session-1".to_owned(),
                turn_id: Some("turn-1".to_owned()),
                emitted_at: 12,
                event_id: 3,
                event: EngineEvent::SessionHandoff {
                    from_session_id: "session-1".to_owned(),
                    to_session_id: "session-2".to_owned(),
                },
            },
        ] {
            observer.observe(&event).expect("event projects");
        }
        let notifications = std::iter::from_fn(|| updates.try_recv().ok())
            .map(|update| {
                app_server_notification(&server, update)
                    .and_then(|bytes| decode_frame(&bytes).map_err(ServerError::from))
                    .expect("notification projects")
            })
            .collect::<Vec<_>>();
        assert_eq!(notifications.len(), 3);
        assert!(matches!(
            &notifications[0],
            Envelope::Notification(Notification { method, params, .. })
                if method == "history/entryAdded" && params["eventId"] == 2
        ));
        assert!(matches!(
            &notifications[1],
            Envelope::Notification(Notification { method, params, .. })
                if method == "history/entryAdded" && params["eventId"] == 3
        ));
        assert!(matches!(
            &notifications[2],
            Envelope::Notification(Notification { method, params, .. })
                if method == "session/compacted"
                    && params["eventId"] == 1
                    && params["sessionId"] == "session-2"
                    && params["oldSessionId"] == "session-1"
                    && params["summaryLength"] == 5
        ));
        let rebound = server.session("session-2").expect("rebound session");
        assert!(
            rebound
                .snapshot
                .expect("handoff snapshot")
                .history
                .iter()
                .all(|entry| entry.metadata().session_id == "session-2")
        );
    }

    #[tokio::test]
    async fn stdio_transport_frames_json_by_newline_and_reports_eof() {
        let (mut client, server) = duplex(1024);
        let (server_read, server_write) = tokio::io::split(server);
        let mut transport = StdioTransport::new(BufReader::new(server_read), server_write);
        client
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{}}\n")
            .await
            .expect("fixture write");
        let frame = transport
            .receive()
            .await
            .expect("frame read")
            .expect("not EOF");
        assert!(decode_frame(&frame).is_ok());
        drop(client);
        assert!(transport.receive().await.expect("EOF read").is_none());
    }

    #[tokio::test]
    async fn stdio_transport_rejects_oversized_frames_before_decoding() {
        let input = vec![b'x'; MAX_FRAME_BYTES.saturating_add(1)];
        let reader = BufReader::new(std::io::Cursor::new(input));
        let writer = tokio::io::sink();
        let mut transport = StdioTransport::new(reader, writer);
        assert!(matches!(
            transport.receive().await,
            Err(TransportError::FrameTooLarge {
                limit: MAX_FRAME_BYTES
            })
        ));
    }

    #[tokio::test]
    async fn streaming_latency_benchmark_observes_every_fake_provider_chunk() {
        let report = benchmark_fake_provider_chunk_latency(128)
            .await
            .expect("streaming benchmark runs");
        assert_eq!(report.chunk_count, 128);
        assert_eq!(report.client_visible_event_count, 128);
        assert!(!report.release_gate_passed);
    }

    #[tokio::test]
    async fn stdio_server_flushes_turn_response_before_deferred_notification() {
        let (client, server_io) = duplex(4096);
        let (client_read, mut client_write) = tokio::io::split(client);
        let (server_read, server_write) = tokio::io::split(server_io);
        let server_task = tokio::spawn(serve_stdio(
            AppServer::default(),
            StdioTransport::new(BufReader::new(server_read), server_write),
            Arc::new(EchoTurnDriver::new("answer")),
        ));
        let mut responses = BufReader::new(client_read).lines();
        for request in [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"test","version":"1","entrypoint":"programmatic","terminalEmulator":"unknown"},"capabilities":{}}}"#,
            r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"session/start","params":{"sessionId":"session-1","workingDirectory":"/workspace"}}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{"sessionId":"session-1","input":[{"type":"text","text":"hello"}]}}"#,
        ] {
            client_write
                .write_all(request.as_bytes())
                .await
                .expect("request bytes");
            client_write
                .write_all(b"\n")
                .await
                .expect("request newline");
        }
        let initialize = responses
            .next_line()
            .await
            .expect("initialize read")
            .expect("initialize response");
        let session = responses
            .next_line()
            .await
            .expect("session read")
            .expect("session response");
        let turn = responses
            .next_line()
            .await
            .expect("turn read")
            .expect("turn response");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&initialize).expect("initialize JSON")["id"],
            1
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&session).expect("session JSON")["id"],
            2
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&turn).expect("turn JSON")["id"],
            3
        );

        let mut notifications = Vec::new();
        loop {
            let notification = responses
                .next_line()
                .await
                .expect("notification read")
                .expect("notification frame");
            let notification = serde_json::from_str::<serde_json::Value>(&notification)
                .expect("notification JSON");
            let completed = notification["method"] == "turn/completed";
            notifications.push(notification);
            assert!(
                notifications.len() <= 8,
                "turn emitted too many notifications"
            );
            if completed {
                break;
            }
        }
        assert_eq!(
            notifications
                .iter()
                .map(|notification| notification["method"].as_str().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec![
                "turn/started",
                "history/entryAdded",
                "history/entryAdded",
                "history/entryUpdated",
                "turn/completed",
            ]
        );
        assert_eq!(
            notifications
                .iter()
                .map(|notification| notification["params"]["eventId"].as_u64())
                .collect::<Vec<_>>(),
            vec![Some(1), Some(2), Some(3), Some(4), Some(5)]
        );
        let assistant = &notifications[2]["params"]["entry"];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["generationStatus"], "in_progress");
        let completion_patch = notifications[3]["params"]["patch"]
            .as_array()
            .expect("completion patch");
        assert!(completion_patch.iter().any(|operation| {
            operation["op"] == "replace"
                && operation["path"] == "/generationStatus"
                && operation["value"] == "completed"
        }));
        drop(client_write);
        drop(responses);
        server_task
            .await
            .expect("server task joins")
            .expect("server exits cleanly");
    }

    #[tokio::test]
    async fn stdio_transport_loss_closes_orphaned_resource_sessions() {
        let backend = Arc::new(CleanupResourceBackend::default());
        let (client, server_io) = duplex(4096);
        let (client_read, mut client_write) = tokio::io::split(client);
        let (server_read, server_write) = tokio::io::split(server_io);
        let server_task = tokio::spawn(serve_stdio(
            AppServer::with_resource_backend(backend.clone()),
            StdioTransport::new(BufReader::new(server_read), server_write),
            Arc::new(EchoTurnDriver::new("answer")),
        ));
        let mut responses = BufReader::new(client_read).lines();
        for request in [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"test","version":"1","entrypoint":"programmatic","terminalEmulator":"unknown"},"capabilities":{}}}"#,
            r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"session/start","params":{"sessionId":"session-1","workingDirectory":"/workspace"}}"#,
        ] {
            client_write
                .write_all(request.as_bytes())
                .await
                .expect("request bytes");
            client_write
                .write_all(b"\n")
                .await
                .expect("request newline");
        }
        assert!(responses.next_line().await.expect("initialize").is_some());
        assert!(responses.next_line().await.expect("session").is_some());
        assert!(backend.opened.load(Ordering::Acquire));
        drop(client_write);
        drop(responses);
        server_task
            .await
            .expect("server task")
            .expect("transport closes");
        assert!(backend.closed.load(Ordering::Acquire));
    }
}

use std::collections::BTreeSet;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use vibe_protocol::ProtocolValidationError;

use crate::client::{DriverError, TurnDriver, TurnReservation, public_turn_error, turn_error_code};
use crate::live_projection::{app_server_notification, app_server_update_channel_for_turn};
use crate::server::{AppServer, DeferredWork, ServerError, server_error_frame};

pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const TASK_CLEANUP_GRACE: Duration = Duration::from_secs(5);

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
    let (events, mut incoming_events) = mpsc::unbounded_channel::<ServeEvent>();
    // A delegated `clientTool/*` request is raised by a tool running on a turn
    // task, so it reaches the wire through the loop rather than by writing to a
    // transport the loop also owns. The retained sender keeps the receiver from
    // completing while the connection is up.
    let (client_tool_frames, mut client_tool_requests) = mpsc::unbounded_channel::<Vec<u8>>();
    connection.client_tools().attach(client_tool_frames.clone());
    let _client_tool_sender = client_tool_frames;
    let mut tasks = JoinSet::new();
    let mut active = BTreeSet::<(String, String)>::new();
    let mut failure = None;
    'serve: loop {
        tokio::select! {
            event = incoming_events.recv() => {
                let Some(event) = event else { break };
                match event {
                    ServeEvent::Frame(bytes) => {
                        if connection.delivers(&bytes)
                            && let Err(error) = transport.send(&bytes).await
                        {
                            failure = Some(error);
                            break 'serve;
                        }
                    }
                    ServeEvent::TurnSettled { session_id, turn_id, notification } => {
                        active.remove(&(session_id, turn_id));
                        match notification {
                            Ok(frames) => {
                                for bytes in frames {
                                    if connection.delivers(&bytes)
                                        && let Err(error) = transport.send(&bytes).await
                                    {
                                        failure = Some(error);
                                        break 'serve;
                                    }
                                }
                            }
                            Err(error) => {
                                failure = Some(TransportError::Server(error));
                                break 'serve;
                            }
                        }
                    }
                    ServeEvent::Failed(error) => {
                        failure = Some(error);
                        break 'serve;
                    }
                }
                while tasks.try_join_next().is_some() {}
            }
            delegated = client_tool_requests.recv() => {
                // A server-to-client request is an answer the client asked for
                // by declaring the capability, so no mute list applies to it.
                if let Some(frame) = delegated
                    && let Err(error) = transport.send(&frame).await
                {
                    failure = Some(error);
                    break 'serve;
                }
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
                    match dispatch_deferred_work(
                        work,
                        &server,
                        &driver,
                        &events,
                        &mut tasks,
                        &mut active,
                    )
                    .await
                    {
                        Ok(frames) => {
                            for bytes in frames {
                                if connection.delivers(&bytes)
                                    && let Err(error) = transport.send(&bytes).await
                                {
                                    failure = Some(error);
                                    break 'serve;
                                }
                            }
                        }
                        Err(error) => {
                            failure = Some(error);
                            break 'serve;
                        }
                    }
                }
                if close_after_work {
                    break;
                }
            }
        }
    }
    // The client is told why the stream stops before it does, which is what the
    // reference sends ahead of dropping a connection its background work broke.
    if let Some(error) = &failure {
        let _ = transport
            .send(&server_error_frame(&error.to_string()))
            .await;
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

/// Performs one unit of deferred work.
///
/// Anything that can block on I/O is spawned so a slow backend never stalls the
/// read loop or the notifications of a turn that is already streaming. The
/// driver controls are cheap and stay inline, where their errors are still
/// fatal to the connection.
///
/// Returns the frames the caller must flush before reading the next request.
async fn dispatch_deferred_work<D>(
    work: DeferredWork,
    server: &AppServer,
    driver: &Arc<D>,
    events: &mpsc::UnboundedSender<ServeEvent>,
    tasks: &mut JoinSet<()>,
    active: &mut BTreeSet<(String, String)>,
) -> Result<Vec<Vec<u8>>, TransportError>
where
    D: TurnDriver + 'static,
{
    match work {
        work @ DeferredWork::RunTurn { .. } => {
            let reservation = server.reserve_turn(work).map_err(TransportError::Server)?;
            active.insert((reservation.session_id.clone(), reservation.turn_id.clone()));
            let server = server.clone();
            let driver = Arc::clone(driver);
            let events = events.clone();
            tasks.spawn(async move { run_turn(server, driver, reservation, events).await });
            Ok(Vec::new())
        }
        DeferredWork::InterruptTurn {
            session_id,
            turn_id,
        } => match driver.interrupt(&session_id, &turn_id) {
            Ok(()) => Ok(Vec::new()),
            Err(error) => {
                let code = turn_error_code(&error);
                server
                    .fail_turn(&session_id, &turn_id, &error.to_string(), code)
                    .map_err(TransportError::Server)
            }
        },
        DeferredWork::SteerTurn {
            session_id,
            turn_id,
            content,
            inject_invoked_skill,
        } => driver
            .steer(&session_id, &turn_id, &content, inject_invoked_skill)
            .map(|()| Vec::new())
            .map_err(TransportError::Driver),
        DeferredWork::InjectContext {
            session_id,
            content,
            as_message,
            inject_invoked_skill,
        } => driver
            .inject_context(&session_id, &content, as_message, inject_invoked_skill)
            .map(|()| Vec::new())
            .map_err(TransportError::Driver),
        DeferredWork::ResolveCallback {
            session_id,
            turn_id,
            callback_id,
            accepted,
            value,
        } => driver
            .resolve_callback(
                &session_id,
                &turn_id,
                &callback_id,
                accepted,
                value.as_deref(),
            )
            .map(|()| Vec::new())
            .map_err(TransportError::Driver),
        DeferredWork::ResourceRequest {
            request_id,
            session_id,
            command,
        } => {
            let server = server.clone();
            spawn_frames(tasks, events.clone(), async move {
                Ok(server
                    .execute_resource_request(request_id, session_id, command)
                    .await
                    .outbound)
            });
            Ok(Vec::new())
        }
        DeferredWork::CloudRequest {
            request_id,
            method,
            params,
        } => {
            let server = server.clone();
            spawn_frames(tasks, events.clone(), async move {
                Ok(server
                    .execute_cloud_request(request_id, method, params)
                    .await
                    .outbound)
            });
            Ok(Vec::new())
        }
        DeferredWork::ConfigureMcp {
            session_id,
            configs,
        } => {
            let server = server.clone();
            spawn_frames(tasks, events.clone(), async move {
                Ok(server.configure_mcp_servers(&session_id, configs).await)
            });
            Ok(Vec::new())
        }
        DeferredWork::CompactSession {
            request_id,
            session_id,
            extra_instructions,
        } => {
            let server = server.clone();
            let driver = Arc::clone(driver);
            spawn_frames(tasks, events.clone(), async move {
                let batch = match driver.compact(&session_id, &extra_instructions).await {
                    Ok(compaction) => server.complete_manual_compaction(
                        request_id,
                        &session_id,
                        &compaction.new_session_id,
                        &compaction.summary,
                        compaction.hydrated,
                    ),
                    Err(error) => {
                        server.fail_manual_compaction(request_id, &session_id, &error.to_string())
                    }
                };
                Ok(batch.outbound)
            });
            Ok(Vec::new())
        }
        DeferredWork::CloseResources {
            session_id,
            generation,
        } => server
            .close_resource_session(&session_id, generation)
            .await
            .map(|()| Vec::new())
            .map_err(TransportError::Server),
    }
}

/// Runs background work whose frames are flushed by the serve loop, in the
/// order the work completes.
fn spawn_frames<F>(tasks: &mut JoinSet<()>, events: mpsc::UnboundedSender<ServeEvent>, work: F)
where
    F: Future<Output = Result<Vec<Vec<u8>>, TransportError>> + Send + 'static,
{
    tasks.spawn(async move {
        match work.await {
            Ok(frames) => {
                for frame in frames {
                    if events.send(ServeEvent::Frame(frame)).is_err() {
                        return;
                    }
                }
            }
            Err(error) => {
                let _ = events.send(ServeEvent::Failed(error));
            }
        }
    });
}

/// Drives one turn to its terminal state, streaming live history updates as the
/// engine produces them.
async fn run_turn<D>(
    server: AppServer,
    driver: Arc<D>,
    reservation: TurnReservation,
    events: mpsc::UnboundedSender<ServeEvent>,
) where
    D: TurnDriver + 'static,
{
    let settle = |notification| ServeEvent::TurnSettled {
        session_id: reservation.session_id.clone(),
        turn_id: reservation.turn_id.clone(),
        notification,
    };
    match server.turn_started(&reservation.session_id, &reservation.turn_id) {
        Ok(frames) => {
            for frame in frames {
                let _ = events.send(ServeEvent::Frame(frame));
            }
        }
        Err(error) => {
            let _ = events.send(settle(Err(error)));
            return;
        }
    }
    let (observer, mut updates) = app_server_update_channel_for_turn(
        reservation.session_id.clone(),
        reservation.turn_id.clone(),
    );
    let mut turn = Box::pin(driver.run_observed(&reservation, observer));
    let outcome = loop {
        tokio::select! {
            outcome = &mut turn => break outcome,
            update = updates.recv() => {
                let Some(update) = update else { continue };
                match app_server_notification(&server, update) {
                    Ok(bytes) => {
                        let _ = events.send(ServeEvent::Frame(bytes));
                    }
                    Err(error) => {
                        let _ = events.send(settle(Err(error)));
                        return;
                    }
                }
            }
        }
    };
    while let Ok(update) = updates.try_recv() {
        match app_server_notification(&server, update) {
            Ok(bytes) => {
                let _ = events.send(ServeEvent::Frame(bytes));
            }
            Err(error) => {
                let _ = events.send(settle(Err(error)));
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
            server.complete_turn_with_details(
                &reservation.session_id,
                &reservation.turn_id,
                outcome.snapshot,
                stop_reason,
                error,
            )
        }
        Err(error) => {
            let code = turn_error_code(&error);
            server.fail_turn(
                &reservation.session_id,
                &reservation.turn_id,
                &error.to_string(),
                code,
            )
        }
    };
    let _ = events.send(settle(notification));
}

/// Something the serve loop must act on once background work reports back.
enum ServeEvent {
    /// A frame to flush to the client.
    Frame(Vec<u8>),
    /// A turn reached a terminal state; its completion frame is attached.
    TurnSettled {
        session_id: String,
        turn_id: String,
        notification: Result<Vec<Vec<u8>>, ServerError>,
    },
    /// Background work failed fatally for this connection.
    Failed(TransportError),
}

async fn fail_deferred(server: &AppServer, deferred: &[DeferredWork], message: &str) {
    for work in deferred {
        match work {
            DeferredWork::RunTurn {
                session_id,
                turn_id,
                ..
            } => {
                // The connection went down before the turn ran; nothing about
                // the request itself failed, which is what leaves it internal.
                let _ = server.fail_turn(
                    session_id,
                    turn_id,
                    message,
                    vibe_core::events::TurnErrorCode::InternalError,
                );
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
mod transport_tests;

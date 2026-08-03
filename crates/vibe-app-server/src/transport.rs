use std::collections::BTreeSet;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use vibe_protocol::ProtocolValidationError;

use crate::client::{DriverError, TurnDriver, TurnReservation, public_turn_error};
use crate::live_projection::{app_server_notification, app_server_update_channel_for_turn};
use crate::server::{AppServer, DeferredWork, ServerError};

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
                                prepared_images: None,
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
                            command,
                        } => {
                            let response = server
                                .execute_resource_request(request_id, session_id, command)
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
    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio::io::{BufReader, duplex};
    use vibe_protocol::decode_frame;

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
            _request: ResourceBackendRequest,
        ) -> ResourceFuture<'a, ResourceDispatch> {
            Box::pin(async move { Err(ResourceError::MethodNotFound("test".to_owned())) })
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

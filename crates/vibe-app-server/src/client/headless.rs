//! The programmatic surface: one process driving sessions without a client.
//!
//! [`HeadlessService`] owns an in-process client and a driver, and turns the
//! calls a program makes into the same dispatch a connected editor would
//! produce. Everything a person would answer interactively is answered here
//! through the interactive tools instead, or refused.
//!
//! A session runs on one of two backends, each a [`SessionBackend`]: the
//! driver behind the in-process connection ([`legacy`]), or the Unified
//! Harness the server selected, which serves the sessions this service opened
//! on it ([`unified`]). A call resolves its session's backend once and hands
//! it the work.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use vibe_core::engine::TurnOutcome;

use super::interactive::queue::CallbackQueue;
use super::interactive::*;
use super::*;

mod legacy;
mod teleport;
pub use teleport::TeleportEvents;
mod unified;

use legacy::LegacySessions;
use unified::UnifiedSessions;

pub struct HeadlessService<D> {
    pub(super) client: InProcessClient,
    pub(super) driver: Arc<D>,
    /// What the interactive tools of an interactive service ask, which the
    /// queue raises as callbacks.
    pub(super) callback_requests: Option<tokio::sync::mpsc::Receiver<InteractiveCallbackRequest>>,
    pub(super) callbacks: CallbackQueue,
    /// The sessions the Unified Harness serves, when the server selected it.
    pub(super) unified: Option<super::unified::UnifiedLink>,
}

type ClientFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ClientError>> + Send + 'a>>;

/// What a turn's engine reports to, and the updates a client reads the turn
/// through.
type TurnUpdates = (
    Arc<dyn EventObserver>,
    tokio::sync::mpsc::Receiver<ProgrammaticUpdate>,
);

/// A field of the state a unified session publishes to its client, reference
/// `ClientState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishedField {
    /// The session record, as `session/updated` replaces it.
    Session,
    /// The history entries, oldest first.
    History,
    /// The child sessions its delegations opened.
    ChildSessions,
}

impl PublishedField {
    /// The field's name in the published state.
    pub(super) const fn key(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::History => "history",
            Self::ChildSessions => "childSessions",
        }
    }
}

/// The calls a session takes on the backend that runs it.
trait SessionBackend {
    /// The session as the server holds it, its history the one the backend
    /// published.
    fn view(&mut self, session_id: &str) -> Result<SessionView, ClientError>;

    /// The session's accounting and its context window, reference
    /// `runtime.stats`.
    fn stats(&mut self, session_id: &str) -> Result<(AgentStatsSnapshot, u64), ClientError>;

    /// One field of the state the session last published to this client
    /// (reference `ClientState`), when it publishes one.
    fn published(&mut self, session_id: &str, field: PublishedField) -> Option<Value>;

    /// Starts `turn`, its images already prepared, and answers the turn it
    /// reserved.
    fn reserve(
        &mut self,
        session_id: &str,
        turn: &TurnRequest,
        images: PreparedImages,
    ) -> Result<TurnReservation, ClientError>;

    /// The loop of `session_id` due at `now_seconds`, reserved, when the
    /// client is the one that fires it.
    fn reserve_due_loop<'a>(
        &'a mut self,
        session_id: &'a str,
        now_seconds: u64,
    ) -> ClientFuture<'a, Option<ScheduledTurn>>;

    /// Settles a reserved turn with the outcome its run produced.
    fn finish(
        &mut self,
        reservation: &TurnReservation,
        outcome: TurnOutcome,
    ) -> Result<ProgrammaticTurn, ClientError>;

    /// Ends a reserved turn as failed, `error` as it is published.
    fn fail(
        &mut self,
        reservation: &TurnReservation,
        error: PublicError,
    ) -> Result<(), ClientError>;

    fn interrupt(
        &mut self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<InterruptOutcome, ClientError>;

    /// Stops the run of `turn_id` where it runs.
    fn stop(&mut self, session_id: &str, turn_id: &str) -> Result<(), ClientError>;

    fn steer(
        &mut self,
        session_id: &str,
        turn_id: &str,
        input: &[PublicContentBlock],
        client_user_message_id: Option<&str>,
    ) -> Result<(), ClientError>;

    fn inject_context(&mut self, session_id: &str, text: &str) -> Result<(), ClientError>;

    fn compact<'a>(
        &'a mut self,
        session_id: &'a str,
        extra_instructions: &'a str,
    ) -> ClientFuture<'a, BTreeMap<String, Value>>;

    fn close<'a>(&'a mut self, session_id: &'a str) -> ClientFuture<'a, ()>;
}

/// Runs a reserved turn to its end, off the service: the driver runs a turn
/// of a legacy session, and a unified session runs its own, which is waited
/// on.
pub struct TurnRunner<D> {
    driver: Arc<D>,
    unified: Option<Arc<super::unified::Shared>>,
}

impl<D> Clone for TurnRunner<D> {
    fn clone(&self) -> Self {
        Self {
            driver: Arc::clone(&self.driver),
            unified: self.unified.clone(),
        }
    }
}

impl<D> TurnRunner<D>
where
    D: TurnDriver,
{
    /// The turn `reservation` holds, run to its end.
    ///
    /// # Errors
    ///
    /// The turn failed, or its driver did.
    pub async fn run(
        &self,
        reservation: &TurnReservation,
        observer: Arc<dyn EventObserver>,
    ) -> Result<TurnOutcome, DriverError> {
        if let Some(shared) = self
            .unified
            .as_ref()
            .filter(|shared| shared.serves(&reservation.session_id))
        {
            return super::unified::settled_turn(
                Arc::clone(shared),
                reservation.session_id.clone(),
                reservation.turn_id.clone(),
            )
            .await;
        }
        self.driver.run_observed(reservation, observer).await
    }
}

impl<D> HeadlessService<D>
where
    D: TurnDriver,
{
    pub fn new(driver: D) -> Result<Self, ClientError> {
        Self::new_shared(Arc::new(driver))
    }

    pub fn new_shared(driver: Arc<D>) -> Result<Self, ClientError> {
        Self::new_shared_with_server(driver, AppServer::default())
    }

    pub fn new_shared_with_server(driver: Arc<D>, server: AppServer) -> Result<Self, ClientError> {
        Self::new_shared_with_server_and_client(
            driver,
            server,
            ClientInfo {
                name: "vibe-programmatic".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                title: None,
                entrypoint: ClientEntrypoint::Programmatic,
                terminal_emulator: TerminalEmulator::Unknown,
            },
            ClientCapabilities::default(),
        )
    }

    pub fn new_shared_with_server_and_client(
        driver: Arc<D>,
        server: AppServer,
        client_info: ClientInfo,
        capabilities: ClientCapabilities,
    ) -> Result<Self, ClientError> {
        let server = server.defaulting_secondary_provider(driver.summary_provider());
        Self::connect(driver, server, client_info, capabilities, None)
    }

    pub fn new_interactive_shared_with_server(
        driver: Arc<D>,
        server: AppServer,
    ) -> Result<Self, ClientError> {
        Self::new_interactive_shared_with_server_and_client(
            driver,
            server,
            ClientInfo {
                name: "vibe-cli".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                title: Some("Mistral Vibe".to_owned()),
                entrypoint: ClientEntrypoint::Cli,
                terminal_emulator: TerminalEmulator::Unknown,
            },
            ClientCapabilities {
                callback_kinds: vec![ClientCallbackKind::Approval, ClientCallbackKind::UserInput],
                ..ClientCapabilities::default()
            },
        )
    }

    pub fn new_interactive_shared_with_server_and_client(
        driver: Arc<D>,
        server: AppServer,
        client_info: ClientInfo,
        mut capabilities: ClientCapabilities,
    ) -> Result<Self, ClientError> {
        if !capabilities
            .callback_kinds
            .contains(&ClientCallbackKind::Approval)
        {
            capabilities
                .callback_kinds
                .push(ClientCallbackKind::Approval);
        }
        let (sender, receiver) =
            tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(MAX_INTERACTIVE_CALLBACKS);
        let channel = CallbackChannel::Shared(sender);
        let plan_directory = driver.plan_directory();
        let server = server
            .defaulting_secondary_provider(driver.summary_provider())
            .using_surface_extension(
                Arc::new(InteractiveApprovalFactory {
                    channel: channel.clone(),
                }),
                Arc::new(InteractiveSessionToolFactory {
                    sender: channel,
                    plan_directory,
                }),
            );
        Self::connect(driver, server, client_info, capabilities, Some(receiver))
    }

    fn connect(
        driver: Arc<D>,
        server: AppServer,
        client_info: ClientInfo,
        capabilities: ClientCapabilities,
        callback_requests: Option<tokio::sync::mpsc::Receiver<InteractiveCallbackRequest>>,
    ) -> Result<Self, ClientError> {
        let unified = server
            .harness_selection()
            .use_unified
            .then(|| super::unified::UnifiedLink::new(client_info.clone(), capabilities.clone()));
        Ok(Self {
            client: InProcessClient::connect_with_server_and_client(
                server,
                client_info,
                capabilities,
            )?,
            driver,
            callback_requests,
            callbacks: CallbackQueue::default(),
            unified,
        })
    }

    /// The backend that runs `session_id`: the Unified Harness for a session
    /// this service opened on it, the driver for any other.
    fn backend(&mut self, session_id: &str) -> Box<dyn SessionBackend + Send + '_> {
        match self.unified.as_mut().filter(|link| link.holds(session_id)) {
            Some(link) => Box::new(UnifiedSessions {
                link,
                server: &self.client.server,
            }),
            // Not `self.legacy_sessions()`: the link the other arm returns
            // stays borrowed here, so this arm borrows the other fields only.
            None => Box::new(LegacySessions {
                client: &mut self.client,
                driver: &self.driver,
                callbacks: &mut self.callbacks,
                requests: &mut self.callback_requests,
            }),
        }
    }

    fn legacy_sessions(&mut self) -> LegacySessions<'_, D> {
        LegacySessions {
            client: &mut self.client,
            driver: &self.driver,
            callbacks: &mut self.callbacks,
            requests: &mut self.callback_requests,
        }
    }

    fn unified_sessions(&mut self) -> Option<UnifiedSessions<'_>> {
        Some(UnifiedSessions {
            link: self.unified.as_mut()?,
            server: &self.client.server,
        })
    }

    /// The unified session a call's parameters name, when they name one.
    fn unified_target(&self, method: &str, params: &Value) -> Option<String> {
        let link = self.unified.as_ref()?;
        match params.get("sessionId").and_then(Value::as_str) {
            Some(session_id) => link.holds(session_id).then(|| session_id.to_owned()),
            // A session-optional read reaches the connection of the session
            // this client opened, as the reference's single host answers it.
            None if !matches!(method, "session/resume" | "session/continue") => link.any_session(),
            None => None,
        }
    }

    pub fn start_session(&mut self, options: &SessionOptions) -> Result<String, ClientError> {
        if let Some(link) = self.unified.as_mut() {
            let (session_id, mcp) = link.start(&self.client.server, options)?;
            if !mcp.is_empty() {
                self.client.pending_mcp.insert(session_id.clone(), mcp);
            }
            return Ok(session_id);
        }
        let session_id = self.client.start_session(options)?;
        self.legacy_sessions().probe_for_titles(&session_id);
        Ok(session_id)
    }

    /// The configuration and session service this session composes over.
    #[must_use]
    pub fn workspace_service(&self) -> crate::workspace::WorkspaceService {
        self.client.workspace_service()
    }

    pub async fn initialize_pending_mcp(
        &mut self,
        session_id: &str,
    ) -> Result<Vec<String>, ClientError> {
        self.client
            .configure_pending_mcp_with_diagnostics(session_id)
            .await
    }

    /// The delegation this service's server sends `clientTool/*` requests
    /// through. Attaching a write side to it is what lets an adapter answer
    /// them for its own client.
    #[must_use]
    pub fn client_tools(&self) -> Arc<crate::client_tools::ClientToolBridge> {
        self.client.server.client_tools.clone()
    }

    #[must_use]
    pub fn driver(&self) -> Arc<D> {
        self.driver.clone()
    }

    /// What runs a reserved turn to its end, off this service.
    #[must_use]
    pub fn turn_runner(&self) -> TurnRunner<D> {
        TurnRunner {
            driver: Arc::clone(&self.driver),
            unified: self.unified.as_ref().map(|link| Arc::clone(&link.shared)),
        }
    }

    /// Whether the Unified Harness serves `session_id`, which answers the
    /// reference's session methods (`session/fork`, `session/history/list`,
    /// ...) where a legacy session answers this port's `internal/` ones.
    #[must_use]
    pub fn is_unified(&self, session_id: &str) -> bool {
        self.unified
            .as_ref()
            .is_some_and(|link| link.holds(session_id))
    }

    pub fn session(&mut self, session_id: &str) -> Result<SessionView, ClientError> {
        self.backend(session_id).view(session_id)
    }

    /// The accounting of `session_id` and its context window, reference
    /// `runtime.stats`: what `stats/read` answers for a legacy session, and
    /// what a unified session last published.
    pub fn stats(&mut self, session_id: &str) -> Result<(AgentStatsSnapshot, u64), ClientError> {
        self.backend(session_id).stats(session_id)
    }

    /// One field of the state `session_id` last published to this client. A
    /// legacy session publishes none.
    pub fn published_state(&mut self, session_id: &str, field: PublishedField) -> Option<Value> {
        self.backend(session_id).published(session_id, field)
    }

    /// Opens a saved session on the Unified Harness, when it is selected and
    /// `method` is a `session/resume` or a `session/continue`.
    fn open_unified(
        &mut self,
        method: &str,
        params: &Value,
    ) -> Option<Result<BTreeMap<String, Value>, ClientError>> {
        if !matches!(method, "session/resume" | "session/continue") {
            return None;
        }
        let link = self.unified.as_mut()?;
        let (session_id, result, deferred) =
            match link.open(&self.client.server, method, params.clone()) {
                Ok(opened) => opened,
                Err(error) => return Some(Err(error)),
            };
        for work in deferred {
            if let DeferredWork::ConfigureMcp { configs, .. } = work {
                self.client.pending_mcp.insert(session_id.clone(), configs);
            }
        }
        Some(Ok(result))
    }

    pub fn public_call(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        if let Some(opened) = self.open_unified(method, &params) {
            return opened;
        }
        if let Some(session_id) = self.unified_target(method, &params)
            && let Some(mut sessions) = self.unified_sessions()
        {
            return sessions.call(&session_id, method, params);
        }
        self.legacy_sessions().call(method, params)
    }

    pub async fn public_call_async(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<PublicDispatch, ClientError> {
        if let Some(opened) = self.open_unified(method, &params) {
            return Ok(PublicDispatch {
                result: opened?,
                notifications: Vec::new(),
            });
        }
        self.begin_public_call(method, params)?.complete().await
    }

    pub fn begin_public_call(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<PendingPublicCall, ClientError> {
        if let Some(session_id) = self.unified_target(method, &params)
            && let Some(mut sessions) = self.unified_sessions()
        {
            return sessions.begin_call(&session_id, method, params);
        }
        self.client.begin_public_call(method, params)
    }

    /// The observer a turn's engine reports to, and the updates a client
    /// reads the turn through. A legacy session's turn counts as started
    /// once its reader is there, as the transport announces it; a unified
    /// session publishes its own turn. Neither needs the service borrowed.
    pub fn turn_updates(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<TurnUpdates, ClientError> {
        match self.unified.as_ref().filter(|link| link.holds(session_id)) {
            Some(link) => unified::turn_updates(link, turn_id),
            None => legacy::turn_updates(&self.client.server, session_id, turn_id),
        }
    }

    /// Reference `_schedule_title_generation_events`: an observer that starts
    /// the background title each completed model step makes due. A client
    /// running a turn on its own composes it with the observer it reads.
    pub fn title_scheduler(&self) -> Arc<dyn EventObserver>
    where
        D: 'static,
    {
        Arc::new(TitleScheduler {
            server: self.client.server.clone(),
            driver: Arc::clone(&self.driver),
        })
    }

    /// The callbacks raised since the last drain: the questions the
    /// interactive tools asked, then those the unified sessions asked.
    pub fn drain_callbacks(&mut self) -> Vec<PublicHistoryEntry> {
        let unified = self
            .unified
            .as_ref()
            .map(super::unified::UnifiedLink::drain_callbacks)
            .unwrap_or_default();
        let mut entries = self.legacy_sessions().raise_callbacks();
        entries.extend(unified);
        entries
    }

    pub fn respond_callback(
        &mut self,
        params: Value,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        self.settle_callback(params, None)
    }

    /// Settles a callback with `params` like [`Self::respond_callback`], but
    /// fails the tool that raised it with `message`. Reference
    /// `TurnController.reject_callback`: an answer the client gave that does
    /// not fit the question.
    pub fn reject_callback(
        &mut self,
        params: Value,
        message: &str,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        self.settle_callback(params, Some(message))
    }

    fn settle_callback(
        &mut self,
        params: Value,
        rejection: Option<&str>,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        let callback_id = params
            .get("callbackId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ClientError::InvalidResponse("callback response omitted callbackId".to_owned())
            })?
            .to_owned();
        let output = params.get("output").cloned().ok_or_else(|| {
            ClientError::InvalidResponse("callback response omitted output".to_owned())
        })?;
        let unified = self
            .unified
            .as_ref()
            .and_then(|link| link.callback_session(&callback_id));
        if let Some(session_id) = unified
            && let Some(mut sessions) = self.unified_sessions()
        {
            return sessions.answer(&session_id, &callback_id, output, rejection);
        }
        self.legacy_sessions()
            .answer(&callback_id, params, output, rejection)
    }

    pub async fn compact(
        &mut self,
        session_id: &str,
        extra_instructions: &str,
    ) -> Result<BTreeMap<String, Value>, ClientError> {
        self.backend(session_id)
            .compact(session_id, extra_instructions)
            .await
    }

    pub async fn reserve_prompt(
        &mut self,
        session_id: &str,
        turn: &TurnRequest,
    ) -> Result<TurnReservation, ClientError> {
        let images = provider_images(&turn.input).await?;
        self.reserve_prepared_prompt(session_id, turn, images).await
    }

    pub async fn reserve_prepared_prompt(
        &mut self,
        session_id: &str,
        turn: &TurnRequest,
        images: PreparedImages,
    ) -> Result<TurnReservation, ClientError> {
        validate_prepared_images(turn, &images)?;
        self.client.configure_pending_mcp(session_id).await?;
        self.backend(session_id).reserve(session_id, turn, images)
    }

    pub async fn reserve_due_loop(
        &mut self,
        session_id: &str,
        now_seconds: u64,
    ) -> Result<Option<ScheduledTurn>, ClientError> {
        self.backend(session_id)
            .reserve_due_loop(session_id, now_seconds)
            .await
    }

    pub fn finish_scheduled_loop(
        &mut self,
        loop_id: &str,
        completed_at_seconds: u64,
    ) -> Result<(), ClientError> {
        self.client
            .server
            .finish_scheduled_loop(loop_id, completed_at_seconds)
            .map_err(ClientError::Server)
    }

    /// Ends a reserved turn as failed, publishing `code` as the reason a client
    /// branches on. [`turn_error_code`] classifies a driver failure into it.
    pub fn fail_reserved(
        &mut self,
        reservation: &TurnReservation,
        message: &str,
        code: TurnErrorCode,
    ) -> Result<(), ClientError> {
        self.fail_reserved_with(reservation, super::public_turn_failure(code, message))
    }

    /// Ends a reserved turn as failed with `error` as it is published, which
    /// [`public_driver_error`] builds out of a driver failure.
    pub fn fail_reserved_with(
        &mut self,
        reservation: &TurnReservation,
        error: PublicError,
    ) -> Result<(), ClientError> {
        self.backend(&reservation.session_id)
            .fail(reservation, error)
    }

    pub async fn prompt(
        &mut self,
        session_id: &str,
        prompt: &str,
    ) -> Result<ProgrammaticTurn, ClientError>
    where
        D: 'static,
    {
        let observer = self.title_scheduler();
        self.run_prompt(session_id, prompt, observer).await
    }

    pub async fn prompt_observed(
        &mut self,
        session_id: &str,
        prompt: &str,
        observer: Arc<dyn EventObserver>,
    ) -> Result<ProgrammaticTurn, ClientError>
    where
        D: 'static,
    {
        let observer = Arc::new(vibe_core::engine::CompositeEventObserver::new(
            observer,
            self.title_scheduler(),
        ));
        self.run_prompt(session_id, prompt, observer).await
    }

    /// Runs a turn of `prompt` where `session_id` runs, `observer` hearing
    /// its events, and settles it.
    async fn run_prompt(
        &mut self,
        session_id: &str,
        prompt: &str,
        observer: Arc<dyn EventObserver>,
    ) -> Result<ProgrammaticTurn, ClientError> {
        let reservation = self
            .reserve_prompt(session_id, &TurnRequest::text(prompt))
            .await?;
        match self.turn_runner().run(&reservation, observer).await {
            Ok(outcome) => self.finish_reserved(&reservation, outcome),
            Err(error) => {
                self.fail_reserved_with(&reservation, public_driver_error(&error))?;
                Err(ClientError::Driver(error))
            }
        }
    }

    pub fn interrupt(
        &mut self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<InterruptOutcome, ClientError> {
        self.backend(session_id).interrupt(session_id, turn_id)
    }

    /// Stops the turn that runs `turn_id` where it runs: the driver of a
    /// legacy session, which settles the canonical turn itself, and the
    /// unified session that runs its own.
    ///
    /// # Errors
    ///
    /// The driver or the session refused.
    pub fn interrupt_driver(&mut self, session_id: &str, turn_id: &str) -> Result<(), ClientError> {
        self.backend(session_id).stop(session_id, turn_id)
    }

    /// Delivers `input` into the running turn: the canonical turn accepts it
    /// first, so a stale or finished turn refuses it before the driver sees it.
    pub fn steer(
        &mut self,
        session_id: &str,
        turn_id: &str,
        input: &[PublicContentBlock],
        client_user_message_id: Option<&str>,
    ) -> Result<(), ClientError> {
        self.backend(session_id)
            .steer(session_id, turn_id, input, client_user_message_id)
    }

    /// Hands `text` to the model as context for the next turn of an idle
    /// session, without a user message of its own.
    pub fn inject_context(&mut self, session_id: &str, text: &str) -> Result<(), ClientError> {
        self.backend(session_id).inject_context(session_id, text)
    }

    pub async fn close_session(&mut self, session_id: &str) -> Result<(), ClientError> {
        self.backend(session_id).close(session_id).await
    }

    pub fn shutdown(&mut self) -> Result<(), ClientError> {
        let result = self.client.shutdown();
        if result.is_ok() {
            self.legacy_sessions()
                .fail_callbacks(None, None, "service was shut down");
        }
        result
    }

    pub fn finish_reserved(
        &mut self,
        reservation: &TurnReservation,
        outcome: TurnOutcome,
    ) -> Result<ProgrammaticTurn, ClientError> {
        self.backend(&reservation.session_id)
            .finish(reservation, outcome)
    }
}

/// Starts the background title a completed model step makes due, beside the
/// rest of the turn.
struct TitleScheduler<D> {
    server: AppServer,
    driver: Arc<D>,
}

impl<D> EventObserver for TitleScheduler<D>
where
    D: TurnDriver + 'static,
{
    fn observe(&self, event: &vibe_core::events::EventEnvelope) -> Result<(), String> {
        if let vibe_core::events::EngineEvent::StepCompleted { turn_completing } = event.event {
            // No wire carries this client's frames: the title lands in the
            // store and the session's state alone.
            super::schedule_title(
                &self.server,
                self.driver.as_ref(),
                &event.session_id,
                turn_completing,
                drop,
            );
        }
        Ok(())
    }
}

//! `session/prompt`: a slash command or a turn, streamed to the client as it
//! runs.
//!
//! Reference `VibeAcpAgent.prompt` and `_forward_event`. Every entry the app
//! server adds or revises is projected through [`crate::projection`], the
//! session title is re-announced when it changes, and the callbacks a turn
//! raises are answered in [`callbacks`].

pub(crate) mod callbacks;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};
use vibe_app_server::client::{
    DriverError, ProgrammaticUpdate, PublicContentBlock, TurnDriver, TurnErrorCode, TurnRequest,
    turn_error_code,
};
use vibe_core::clock::now_millis;
use vibe_core::engine::{TurnOutcome, TurnStopReason};
use vibe_core::provider::ProviderError;
use vibe_core::session_id::uuid_v4;

use crate::agent::AcpAgent;
use crate::commands::CommandOutcome;
use crate::content::{ProjectedPrompt, project_prompt, validate_display_content};
use crate::projection::{added_entry_updates, session_info_update, updated_entry_updates};
use crate::protocol::AcpError;
use crate::session::AcpHarness;
use crate::session::harness::ActivePhase;

/// Interval at which the canonical callback queue is drained while a turn runs.
const CALLBACK_POLL: Duration = Duration::from_millis(5);

/// The `_meta` key a client carries the rendered form of its prompt under.
/// Reference `USER_DISPLAY_CONTENT_META_KEY`.
const USER_DISPLAY_CONTENT_META_KEY: &str = "user_display_content";

/// What a turn runs on.
struct TurnInput {
    text: String,
    client_message_id: Option<String>,
    images: Vec<Value>,
    resources: Vec<Value>,
    display: Option<Value>,
    injected: bool,
}

/// How a turn ended.
enum TurnEnd {
    Completed(vibe_app_server::client::ProgrammaticTurn),
    Cancelled,
}

impl<D> AcpAgent<D>
where
    D: TurnDriver + 'static,
{
    /// Reference `prompt`.
    pub async fn prompt_content(
        self: &Arc<Self>,
        session_id: &str,
        prompt: Vec<Value>,
        meta: &Map<String, Value>,
    ) -> Result<Value, AcpError> {
        let harness = self.session_harness(session_id)?;
        let display = match meta.get(USER_DISPLAY_CONTENT_META_KEY) {
            None | Some(Value::Null) => None,
            Some(display) => {
                validate_display_content(display).map_err(|error| {
                    AcpError::InvalidParams(format!(
                        "the user display content metadata is invalid: {error}"
                    ))
                })?;
                Some(display.clone())
            }
        };
        let content = project_prompt(&prompt)?;
        let message_id = uuid_v4();
        let input =
            match crate::commands::execute(self, &harness, &content.text, &message_id).await? {
                Some(CommandOutcome::Response(response)) => return Ok(response),
                Some(CommandOutcome::Injected(text)) => TurnInput {
                    text,
                    client_message_id: None,
                    images: Vec::new(),
                    resources: Vec::new(),
                    display: None,
                    injected: true,
                },
                None => {
                    self.record_skill_command(&harness, &content.text).await;
                    turn_input(content, message_id, display)
                }
            };
        self.run_prompt_turn(&harness, input).await
    }

    /// Reference `cancel`: the prompt the session runs stops, and a turn it
    /// already started is interrupted.
    pub async fn cancel(&self, session_id: &str) -> Result<(), AcpError> {
        let harness = self.session_harness(session_id)?;
        self.record(
            &harness,
            "vibe.user_cancelled_action",
            json!({"action": "interrupt_agent"}),
        )
        .await;
        if let ActivePhase::Running(turn_id) = harness.request_cancel() {
            let _ = harness
                .service
                .lock()
                .await
                .interrupt_driver(&harness.session_id, &turn_id);
        }
        Ok(())
    }

    async fn run_prompt_turn(
        self: &Arc<Self>,
        harness: &Arc<AcpHarness<D>>,
        input: TurnInput,
    ) -> Result<Value, AcpError> {
        harness.begin(ActivePhase::Reserving)?;
        let outcome = self.run_turn(harness, input).await;
        harness.release();
        let end = match outcome {
            Ok(end) => end,
            Err(TurnFailure::Turn(code, error)) => {
                if code == TurnErrorCode::ResponseTooLong {
                    self.send_usage_update(harness);
                    return Ok(json!({
                        "stopReason": "max_tokens",
                        "usage": self.usage(harness).await,
                    }));
                }
                return Err(error);
            }
            Err(TurnFailure::Other(error)) => return Err(error),
        };
        self.send_usage_update(harness);
        let turn = match end {
            TurnEnd::Cancelled => {
                return Ok(json!({"stopReason": "cancelled", "usage": self.usage(harness).await}));
            }
            TurnEnd::Completed(turn) => turn,
        };
        let stop_reason = match turn.stop_reason {
            TurnStopReason::Cancelled => "cancelled",
            reason if reason.is_limit() => "max_turn_requests",
            _ => "end_turn",
        };
        let usage = self.usage(harness).await;
        if stop_reason != "end_turn" {
            return Ok(json!({"stopReason": stop_reason, "usage": usage}));
        }
        let mut response = json!({"stopReason": "end_turn", "usage": usage});
        if self.should_show_feedback(harness).await {
            response["_meta"] = json!({"show_feedback_prompt": true});
        }
        Ok(response)
    }

    /// Forwards one update of the running turn, with the usage report a
    /// statistics change left pending once a tool call starts.
    async fn route_update(
        &self,
        harness: &AcpHarness<D>,
        update: ProgrammaticUpdate,
        usage_pending: &mut bool,
    ) {
        // A Unified Harness session reports each accounting it publishes, as
        // reference `_forward_event` answers every `StatsUpdated`.
        if let ProgrammaticUpdate::Accounting {
            stats,
            context_window,
        } = &update
        {
            self.session_update(
                &harness.session_id,
                super::surface::usage_update_of(stats, *context_window),
            );
            *usage_pending = false;
            return;
        }
        if matches!(update, ProgrammaticUpdate::Stats { .. }) && self.harness.use_unified {
            return;
        }
        if matches!(update, ProgrammaticUpdate::Stats { .. }) {
            *usage_pending = true;
            return;
        }
        if *usage_pending && starts_effect(&update) {
            *usage_pending = false;
            let usage = self.usage_update(harness).await;
            self.session_update(&harness.session_id, usage);
        }
        self.forward_update(harness, update);
    }

    /// Reference `feedback.should_show`, recorded as asked when it answers yes.
    async fn should_show_feedback(&self, harness: &AcpHarness<D>) -> bool {
        let show = self
            .call(
                harness,
                "feedback/shouldShow",
                json!({"pendingUserMessages": 1}),
            )
            .await
            .ok()
            .and_then(|result| result.get("show").and_then(Value::as_bool))
            .unwrap_or(false);
        if show {
            let _ = self
                .call(harness, "feedback/record", json!({"event": "asked"}))
                .await;
        }
        show
    }

    async fn run_turn(
        self: &Arc<Self>,
        harness: &Arc<AcpHarness<D>>,
        input: TurnInput,
    ) -> Result<TurnEnd, TurnFailure> {
        let mut blocks = vec![PublicContentBlock::Text {
            text: input.text.clone(),
        }];
        blocks.extend(
            input
                .images
                .into_iter()
                .map(|attachment| PublicContentBlock::Image { attachment }),
        );
        blocks.extend(
            input
                .resources
                .into_iter()
                .map(|resource| PublicContentBlock::Resource { resource }),
        );
        let request = TurnRequest {
            idempotency_key: None,
            prompt: input.text,
            input: blocks,
            injected: input.injected,
            client_user_message_id: input.client_message_id,
            auto_title: None,
            user_display_content: input.display,
            mention_stats: None,
        };
        // Reference `_announce_turn_started` publishes the statistics as the
        // turn opens, before the turn counts as a step; a Unified Harness
        // session publishes its own once the turn has started.
        let opening_usage = if self.harness.use_unified {
            None
        } else {
            Some(self.usage_update(harness).await)
        };
        let (reservation, driver, observer, mut updates) = {
            let mut service = harness.service.lock().await;
            let reservation = service
                .reserve_prompt(&harness.session_id, &request)
                .await
                .map_err(|error| TurnFailure::Other(reserve_error(error)))?;
            if let Some(opening_usage) = opening_usage {
                self.session_update(&harness.session_id, opening_usage);
            }
            let channel = service.turn_updates(&reservation.session_id, &reservation.turn_id);
            let (observer, updates) = match channel {
                Ok(channel) => channel,
                Err(error) => {
                    let _ = service.fail_reserved(
                        &reservation,
                        &error.to_string(),
                        TurnErrorCode::InternalError,
                    );
                    return Err(TurnFailure::Other(error.into()));
                }
            };
            let runner = service.turn_runner(&reservation.session_id);
            (reservation, runner, observer, updates)
        };
        harness.set_phase(ActivePhase::Running(reservation.turn_id.clone()));
        if harness.is_cancelled() {
            let _ = harness
                .service
                .lock()
                .await
                .interrupt_driver(&reservation.session_id, &reservation.turn_id);
        }
        let mut callback_poll = tokio::time::interval(CALLBACK_POLL);
        callback_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let run = driver.run(&reservation, observer);
        tokio::pin!(run);
        // Reference `_forward_event` answers `StatsUpdated` with a usage update
        // from a task of its own, which reads the statistics only once the
        // prompt awaits something: a tool the model asked for, or the end of
        // the turn. A usage report waits for the same moment here.
        let mut usage_pending = false;
        let (mut flushes, _barrier) = harness.barrier.install();
        let outcome: Result<TurnOutcome, DriverError> = loop {
            tokio::select! {
                biased;
                outcome = &mut run => break outcome,
                update = updates.recv() => {
                    if let Some(update) = update {
                        self.route_update(harness, update, &mut usage_pending).await;
                    }
                }
                Some(acknowledge) = flushes.recv() => {
                    while let Ok(update) = updates.try_recv() {
                        self.route_update(harness, update, &mut usage_pending).await;
                    }
                    let _ = acknowledge.send(());
                }
                _ = callback_poll.tick() => {
                    // Everything the turn raised before the question reaches
                    // the client before the question does.
                    while let Ok(update) = updates.try_recv() {
                        self.route_update(harness, update, &mut usage_pending).await;
                    }
                    if let Err(error) = self.route_pending_callbacks(harness).await {
                        let mut service = harness.service.lock().await;
                        let _ = service.interrupt_driver(&reservation.session_id, &reservation.turn_id);
                        let _ = service.fail_reserved(
                            &reservation,
                            &error.to_string(),
                            TurnErrorCode::InternalError,
                        );
                        return Err(TurnFailure::Other(error));
                    }
                }
            }
        };
        // What the turn raised before it ended still reaches the client the
        // way the loop routes it, accounting included.
        while let Ok(update) = updates.try_recv() {
            self.route_update(harness, update, &mut usage_pending).await;
        }
        let cancelled = harness.is_cancelled();
        match outcome {
            Ok(outcome) => {
                let turn = harness
                    .service
                    .lock()
                    .await
                    .finish_reserved(&reservation, outcome)
                    .map_err(|error| TurnFailure::Other(error.into()))?;
                if cancelled {
                    return Ok(TurnEnd::Cancelled);
                }
                Ok(TurnEnd::Completed(turn))
            }
            Err(error) => {
                let code = turn_error_code(&error);
                let _ = harness.service.lock().await.fail_reserved(
                    &reservation,
                    &error.to_string(),
                    code,
                );
                if cancelled {
                    return Ok(TurnEnd::Cancelled);
                }
                // The failed turn still publishes its statistics.
                self.send_usage_update(harness);
                let mapped = self.driver_error(harness, &error, code).await;
                Err(TurnFailure::Turn(code, mapped))
            }
        }
    }

    /// Reference `from_public_error`: the ACP error a failed turn becomes.
    async fn driver_error(
        &self,
        harness: &AcpHarness<D>,
        error: &DriverError,
        code: TurnErrorCode,
    ) -> AcpError {
        let unauthorized = matches!(
            error,
            DriverError::MissingCredentialEnvironment(_)
                | DriverError::Provider(ProviderError::HttpStatus { status: 401 | 403 })
                | DriverError::Engine(vibe_core::engine::EngineError::Provider(
                    ProviderError::HttpStatus { status: 401 | 403 }
                ))
        );
        if unauthorized {
            return AcpError::Configuration(error.to_string());
        }
        // Reference `from_public_error` reads the provider, the model and a
        // refusal's reasons off the published details; a failure that carries
        // none names the active model.
        let details = vibe_app_server::client::turn_error_details(error);
        let detail = |name: &str| details.get(name).and_then(Value::as_str).map(str::to_owned);
        let (active_provider, active_model) = self.active_provider(harness).await;
        let provider = detail("provider").unwrap_or(active_provider);
        let model = detail("model").unwrap_or(active_model);
        match code {
            TurnErrorCode::RateLimit => AcpError::RateLimited { provider, model },
            TurnErrorCode::ContextTooLong => AcpError::ContextTooLong { provider, model },
            TurnErrorCode::ResponseTooLong => AcpError::ConversationLimit(error.to_string()),
            TurnErrorCode::Refusal => AcpError::Refusal {
                provider,
                model,
                category: detail("category"),
                explanation: detail("explanation"),
            },
            TurnErrorCode::InvalidModel | TurnErrorCode::InvalidApiKey => {
                AcpError::Configuration(error.to_string())
            }
            TurnErrorCode::InvalidImageAttachment => AcpError::InvalidImage {
                detail: error.to_string(),
                reason: "invalid_image_attachment".to_owned(),
            },
            TurnErrorCode::ImagesNotSupported => AcpError::ImagesNotSupported(model),
            TurnErrorCode::CompactionFailed => AcpError::CompactionFailed {
                reason: "failed".to_owned(),
                detail: error.to_string(),
            },
            TurnErrorCode::IncompleteStream
            | TurnErrorCode::BackendError
            | TurnErrorCode::InternalError => AcpError::Internal(error.to_string()),
        }
    }

    /// The provider name and model name of the active model.
    async fn active_provider(&self, harness: &AcpHarness<D>) -> (String, String) {
        let config = self.config_view(harness).await.unwrap_or(Value::Null);
        let model = config
            .pointer("/activeModel/name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let provider = config
            .pointer("/activeModel/provider")
            .and_then(Value::as_str)
            .unwrap_or("mistral")
            .to_owned();
        (provider, model)
    }

    /// Projects one update the app server published while a turn ran.
    pub(crate) fn forward_update(&self, harness: &AcpHarness<D>, update: ProgrammaticUpdate) {
        if let ProgrammaticUpdate::Renamed { title, preview } = &update {
            if let Some(shown) = harness.observe_names(title.as_deref(), preview.as_deref()) {
                self.session_update(
                    &harness.session_id,
                    session_info_update(Some(&shown), now_millis()),
                );
            }
            return;
        }
        let ProgrammaticUpdate::HistoryEntry { entry, .. } = update else {
            return;
        };
        let Ok(value) = serde_json::to_value(entry.as_ref()) else {
            return;
        };
        let updates = match harness.remember_entry(&value) {
            Some(previous) => updated_entry_updates(&previous, &value),
            None => added_entry_updates(&value),
        };
        for update in updates {
            self.session_update(&harness.session_id, update);
        }
        if let Some(title) = harness.observe_display_title(&entry) {
            self.session_update(
                &harness.session_id,
                session_info_update(Some(&title), now_millis()),
            );
        }
    }
}

/// Whether `update` is a tool call starting, which is where the reference's
/// prompt first waits on something after a model response.
fn starts_effect(update: &ProgrammaticUpdate) -> bool {
    let ProgrammaticUpdate::HistoryEntry { entry, .. } = update else {
        return false;
    };
    matches!(
        entry.as_ref(),
        vibe_app_server::client::PublicHistoryEntry::Effect { .. }
    )
}

/// Why a turn did not end.
enum TurnFailure {
    /// The turn failed with a code a client branches on.
    Turn(TurnErrorCode, AcpError),
    /// Something around the turn failed.
    Other(AcpError),
}

/// A reservation the app server refused: a session already running a turn is
/// what the reference reports as an internal error.
fn reserve_error(error: vibe_app_server::client::ClientError) -> AcpError {
    AcpError::Internal(error.to_string())
}

fn turn_input(content: ProjectedPrompt, message_id: String, display: Option<Value>) -> TurnInput {
    TurnInput {
        text: content.text,
        client_message_id: Some(message_id),
        images: content.images,
        resources: content.resources,
        display,
        injected: false,
    }
}

//! The turn's cycle: open it, ask the budget policies, call the model, run
//! the reply's tools, and settle on a stop reason.
//!
//! Everything that happens inside one cycle is delegated to the sibling
//! modules; what is left here is the order the reference's run loop
//! (`vibe/core/agent_loop/_loop.py`) asks its questions in, and what a turn
//! injects for the operator's own message: the skill a slash command invokes
//! and the files an `@` mention names.

use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use super::controls::{CancellationToken, ControlOutcome, Steering, TurnControlHandle};
use super::ledger::{
    TurnLedger, TurnRecorder, lifecycle_for, persist, persist_stats, stop_message,
};
use super::streaming::{StreamOutcome, request_sent, stamp_census};
use super::tool_round::ToolRound;
use super::{
    Compactor, CompletionProvider, ConversationEngine, EngineError, ModelCallStats, ToolExecutor,
    TranscriptSink, TurnOutcome, TurnStopReason,
};
use crate::events::{
    EffectApproval, EffectDetail, EngineEvent, LifecycleState, ModelMessage, ModelToolCall,
    PersistedToolResult,
};
use crate::hooks::{HookInvocation, HookYield};
use crate::middleware::{
    AutoCompactMiddleware, ConversationContext, MiddlewareAction, MiddlewarePipeline, ResetReason,
};
use crate::provider::{ProviderError, ProviderInput, Usage};
use crate::tracing::{AgentSpan, agent_span};

impl<P, T, C, S> ConversationEngine<P, T, C, S>
where
    P: CompletionProvider,
    T: ToolExecutor,
    C: Compactor,
    S: TranscriptSink,
{
    pub async fn run_turn(
        &self,
        session_id: impl Into<String>,
        input: ProviderInput,
        prompt: impl Into<String>,
        cancellation: CancellationToken,
    ) -> Result<TurnOutcome, EngineError> {
        self.run_turn_controlled(
            session_id,
            input,
            prompt,
            cancellation,
            TurnControlHandle::default(),
        )
        .await
    }

    /// Runs one turn under the span the whole trace hangs off. Reference
    /// `_loop.py` opens `agent_span` around its run loop, which is what puts
    /// every model call, tool call and hook of the turn under one parent and
    /// publishes the conversation id the descendants read back.
    pub async fn run_turn_controlled(
        &self,
        session_id: impl Into<String>,
        input: ProviderInput,
        prompt: impl Into<String>,
        cancellation: CancellationToken,
        controls: TurnControlHandle,
    ) -> Result<TurnOutcome, EngineError> {
        let session_id = session_id.into();
        let model = self.resolved_model(&input);
        agent_span(
            AgentSpan {
                model: model.as_deref(),
                session_id: Some(&session_id),
            },
            self.run_traced_turn(
                session_id.clone(),
                input,
                prompt.into(),
                cancellation,
                controls,
            ),
        )
        .await
    }

    async fn run_traced_turn(
        &self,
        session_id: String,
        mut input: ProviderInput,
        prompt: String,
        cancellation: CancellationToken,
        controls: TurnControlHandle,
    ) -> Result<TurnOutcome, EngineError> {
        let mut recorder = TurnRecorder::new(
            self.settings.observer.as_ref(),
            session_id,
            input.turn_id.as_deref(),
            self.settings.working_directory.clone(),
        );
        let mut messages = input.messages.clone();
        // Reference `act` cleans the history before a turn opens: a call an
        // earlier interrupt left unanswered is answered as interrupted.
        crate::events::fill_missing_tool_responses(&mut messages);
        let mut ledger = TurnLedger::new(&self.settings.baseline, &self.settings.limits);
        let mut checkpoints = 0_u32;
        // Reference `_reactive_recovery_used`, reset at the top of every run:
        // an overflow is recovered from at most once per user turn, so a
        // transcript that still overflows after a compaction is reported
        // instead of compacting again.
        let mut reactive_recovery_used = false;
        // Reference `first_llm_turn`: the request that answers the operator is
        // the main call until one completes, an overflow retry included.
        let mut first_call = true;
        // The budget policies answer at the top of every cycle, as part of the
        // full pipeline.
        let mut pipeline = MiddlewarePipeline::from_limits(&self.settings.limits);
        // Automatic compaction is registered after the budget policies and
        // before anything a caller added, which is the reference's order in
        // `_setup_middleware`: a cycle that reaches a limit and the threshold at
        // once stops instead of compacting.
        pipeline.add(Arc::new(AutoCompactMiddleware));
        for middleware in &self.settings.middleware {
            pipeline.add(Arc::clone(middleware));
        }
        pipeline.reset(ResetReason::Stop);

        if self.settings.injected_prompt {
            recorder.emit(EngineEvent::Lifecycle {
                state: LifecycleState::Running,
                message: None,
            })?;
            // Reference `_open_user_turn` keeps what was attached to an
            // injected turn as it does for the operator's.
            messages.push(ModelMessage::User {
                content: prompt.clone(),
                injected: true,
                message_id: None,
                attachments: self.settings.user_attachments.clone(),
                user_display_content: self.settings.user_display_content.clone(),
                input_text: self.settings.user_input_text.clone(),
                manual_shell: None,
                compaction_boundary: false,
            });
        } else {
            recorder.emit(EngineEvent::UserMessage {
                content: self
                    .settings
                    .user_input_text
                    .clone()
                    .unwrap_or_else(|| prompt.clone()),
                message_id: self.settings.user_message_id.clone(),
                attachments: self.settings.user_attachments.clone(),
                user_display_content: self.settings.user_display_content.clone(),
            })?;
        }
        // Reference `_current_user_message_id`: every request and tool event of
        // this turn reports the operator's message, as the projection published
        // it.
        let message_id = if self.settings.injected_prompt {
            recorder.last_entry_id()
        } else {
            recorder.last_history_entry_id()
        };
        recorder.emit(EngineEvent::TurnOpened {
            model: self.resolved_model(&input).unwrap_or_default(),
            agent_profile: self.settings.agent_profile.clone(),
            message_id: message_id.clone(),
        })?;
        if !self.settings.injected_prompt {
            // Reference `_open_user_turn`: the operator's message is a step of
            // the session, which is what the turn budget subtracts back out.
            ledger.count_user_message();
            messages.push(ModelMessage::User {
                content: prompt.clone(),
                injected: false,
                message_id: message_id.clone(),
                attachments: self.settings.user_attachments.clone(),
                user_display_content: self.settings.user_display_content.clone(),
                input_text: self.settings.user_input_text.clone(),
                manual_shell: None,
                compaction_boundary: false,
            });
            // Reference `prepare_prompt_from_context` leaves the title unset:
            // a turn names its session only through a generated or supplied
            // title, never from the prompt itself.
            self.inject_invoked_skill(&mut recorder, &mut messages, &prompt)?;
            if let Some(message) = self
                .inject_mentioned_files(&mut recorder, &mut messages, &prompt, &cancellation)
                .await?
            {
                recorder.emit(EngineEvent::Lifecycle {
                    state: LifecycleState::Failed,
                    message: Some(message.clone()),
                })?;
                persist(&self.sink, &mut messages, recorder.state()).await?;
                return Err(EngineError::ToolFailure(message));
            }
        }
        // Reference `_open_user_turn`: every user turn starts with its full
        // allowance of post-agent retries.
        if let Some(hooks) = &self.settings.hooks {
            hooks.manager.reset_retry_count();
        }
        persist(&self.sink, &mut messages, recorder.state()).await?;
        checkpoints = checkpoints.saturating_add(1);

        let stop_reason = loop {
            // Cancellation answers before any policy does: a cancelled turn has
            // no budget question left to ask.
            if cancellation.is_cancelled() {
                break TurnStopReason::Cancelled;
            }
            match self
                .apply_controls(
                    &mut recorder,
                    &mut messages,
                    &mut input,
                    &controls,
                    &cancellation,
                )
                .await?
            {
                ControlOutcome::Stop(reason) => break reason,
                ControlOutcome::Failed(message) => {
                    recorder.emit(EngineEvent::Lifecycle {
                        state: LifecycleState::Failed,
                        message: Some(message.clone()),
                    })?;
                    persist(&self.sink, &mut messages, recorder.state()).await?;
                    return Err(EngineError::ToolFailure(message));
                }
                // A rotated session is only durable once the transcript lands
                // under its new identifier, so it checkpoints before the next
                // request rather than at the end of the cycle.
                ControlOutcome::Continue(true) => {
                    persist(&self.sink, &mut messages, recorder.state()).await?;
                    checkpoints = checkpoints.saturating_add(1);
                }
                ControlOutcome::Continue(false) => {}
            }
            let policy = pipeline.before_turn(&ConversationContext {
                messages: &messages,
                stats: &ledger.session_stats(recorder.tool_calls()),
                price_micros: ledger.price_micros,
                compaction: &self.settings.compaction,
            });
            match policy.action {
                // A policy that stops without naming a public status ends the
                // turn the way a finished conversation does.
                MiddlewareAction::Stop => {
                    // Reference `_handle_middleware_result`: the reason is
                    // published as an assistant message wrapped in the stop
                    // tag, which is what a programmatic run reports.
                    if let Some(reason) = &policy.reason {
                        let (text_id, _) = recorder.open_model_call();
                        recorder.emit(EngineEvent::ModelText {
                            text: format!("<vibe_stop_event>{reason}</vibe_stop_event>"),
                            message_id: Some(text_id),
                        })?;
                    }
                    break policy.stop_reason.unwrap_or(TurnStopReason::Complete);
                }
                MiddlewareAction::Compact => {
                    match self
                        .compact(
                            &mut recorder,
                            &mut messages,
                            message_id.clone(),
                            &pipeline,
                            &mut ledger,
                            &cancellation,
                        )
                        .await?
                    {
                        // The reference does not re-enter the pipeline after a
                        // compaction: the policy that asked for it would read
                        // the same context size and ask again.
                        Some(()) => {
                            persist(&self.sink, &mut messages, recorder.state()).await?;
                            checkpoints = checkpoints.saturating_add(1);
                        }
                        None => break TurnStopReason::Cancelled,
                    }
                }
                // Reference `_handle_middleware_result` appends the message
                // and publishes nothing: the history a client reads leaves a
                // harness reminder out.
                MiddlewareAction::InjectMessage => {
                    if let Some(content) = policy.message {
                        messages.push(ModelMessage::injected_user(content));
                    }
                }
                MiddlewareAction::Continue => {}
            }
            // The model reads from the latest compaction envelope on; the
            // transcript keeps what came before it.
            input.messages = crate::compaction::context::select_model_context(&messages);
            input.session_id = Some(recorder.state().session_id.clone());
            let call_type = if first_call {
                crate::telemetry::TelemetryCallType::MainCall
            } else {
                crate::telemetry::TelemetryCallType::SecondaryCall
            };
            stamp_census(
                self.settings.request_census.as_ref(),
                &mut input,
                &recorder.state().session_id,
                call_type,
                &message_id,
            );
            recorder.emit(request_sent(
                &input,
                self.resolved_model(&input).unwrap_or_default(),
                self.settings.agent_profile.clone(),
                message_id.clone(),
                call_type,
            ))?;
            let call_started = Instant::now();
            let completion = match self
                .stream_completion(
                    &mut recorder,
                    &input,
                    &cancellation,
                    Some(Steering {
                        controls: &controls,
                        messages: &mut messages,
                    }),
                )
                .await?
            {
                StreamOutcome::Cancelled => break TurnStopReason::Cancelled,
                StreamOutcome::SteerFailed(message) => {
                    recorder.emit(EngineEvent::Lifecycle {
                        state: LifecycleState::Failed,
                        message: Some(message.clone()),
                    })?;
                    persist(&self.sink, &mut messages, recorder.state()).await?;
                    return Err(EngineError::ToolFailure(message));
                }
                StreamOutcome::Completed(Ok(completion)) => *completion,
                StreamOutcome::Completed(Err(error)) if error.is_context_overflow() => {
                    // Reference `_should_self_heal`: one recovery per turn, and
                    // none at all in strict mode, where the operator asked for
                    // the overflow rather than for a silent repair.
                    if reactive_recovery_used
                        || self.settings.compaction.raise_on_compaction_failure
                    {
                        recorder.emit(EngineEvent::Lifecycle {
                            state: LifecycleState::Failed,
                            message: Some(error.to_string()),
                        })?;
                        persist(&self.sink, &mut messages, recorder.state()).await?;
                        return Err(EngineError::Provider(error));
                    }
                    reactive_recovery_used = true;
                    match self
                        .compact(
                            &mut recorder,
                            &mut messages,
                            message_id.clone(),
                            &pipeline,
                            &mut ledger,
                            &cancellation,
                        )
                        .await?
                    {
                        Some(()) => {
                            persist(&self.sink, &mut messages, recorder.state()).await?;
                            checkpoints = checkpoints.saturating_add(1);
                            continue;
                        }
                        None => break TurnStopReason::Cancelled,
                    }
                }
                StreamOutcome::Completed(Err(ProviderError::Refusal(_))) => {
                    break TurnStopReason::Refusal;
                }
                StreamOutcome::Completed(Err(error)) => {
                    // Reference `_chat_streaming`: a refused answer is kept
                    // whole and counted, an interrupted one keeps its text.
                    if let ProviderError::Call(call) = &error
                        && let Some(appended) = &call.appended
                    {
                        if appended.usage != Usage::default() {
                            ledger.record_completion(&appended.usage, &self.settings.limits);
                            recorder.emit(EngineEvent::Stats {
                                context_tokens: ledger.context_tokens,
                                input_tokens: ledger.usage.input_tokens,
                                output_tokens: ledger.usage.output_tokens,
                                cached_tokens: ledger.usage.cached_tokens,
                            })?;
                        }
                        let (assistant_id, reasoning_id) = recorder.model_call_entries();
                        messages.push(ModelMessage::Assistant {
                            message_id: assistant_id,
                            reasoning_message_id: reasoning_id,
                            content: appended.text.clone(),
                            reasoning: appended.reasoning.clone(),
                            reasoning_payloads: appended.reasoning_payloads.clone(),
                            tool_calls: appended.tool_calls.clone(),
                            keeps_empty_content: false,
                        });
                    }
                    recorder.emit(EngineEvent::Lifecycle {
                        state: LifecycleState::Failed,
                        message: Some(error.to_string()),
                    })?;
                    persist(&self.sink, &mut messages, recorder.state()).await?;
                    return Err(EngineError::Provider(error));
                }
            };

            first_call = false;
            ledger.record_completion(&completion.usage, &self.settings.limits);
            ledger.last_call = Some(ModelCallStats {
                prompt_tokens: completion.usage.input_tokens,
                completion_tokens: completion.usage.output_tokens,
                cached_tokens: completion.usage.cached_tokens,
                duration_ms: u64::try_from(call_started.elapsed().as_millis())
                    .unwrap_or(u64::MAX)
                    .max(1),
            });
            recorder.emit(EngineEvent::Stats {
                context_tokens: ledger.context_tokens,
                input_tokens: ledger.usage.input_tokens,
                output_tokens: ledger.usage.output_tokens,
                cached_tokens: ledger.usage.cached_tokens,
            })?;
            let (assistant_id, reasoning_id) = recorder.model_call_entries();
            let assistant_message = ModelMessage::Assistant {
                message_id: assistant_id,
                reasoning_message_id: reasoning_id,
                content: completion.text.clone(),
                reasoning: completion.reasoning.clone(),
                reasoning_payloads: completion.reasoning_payloads.clone(),
                tool_calls: completion.tool_calls.clone(),
                keeps_empty_content: false,
            };
            // The budgets answer at the top of the next cycle only, as
            // reference `_conversation_loop` asks them: a reply's tool calls
            // run before a limit it reached stops the turn. A cancellation
            // keeps the reply as `_chat_streaming` appended it; calls it left
            // unanswered are answered when the next turn opens.
            if cancellation.is_cancelled() {
                messages.push(assistant_message);
                break TurnStopReason::Cancelled;
            }
            if completion.text.len() > self.settings.limits.max_response_bytes {
                break TurnStopReason::ResponseLength;
            }
            messages.push(assistant_message);
            if completion.tool_calls.is_empty() {
                ledger.count_step();
                persist(&self.sink, &mut messages, recorder.state()).await?;
                recorder.emit(EngineEvent::StepCompleted {
                    turn_completing: true,
                })?;
                // Reference `_conversation_loop`: a turn about to end runs the
                // post-agent hooks, and one that denies sends the model back
                // with its reason as an injected user message.
                if let Some(retry) = self.run_post_agent_hooks(&mut recorder).await? {
                    messages.push(ModelMessage::injected_user(retry));
                    persist(&self.sink, &mut messages, recorder.state()).await?;
                    checkpoints = checkpoints.saturating_add(1);
                    continue;
                }
                break TurnStopReason::Complete;
            }

            let (user_cancelled, injections) = match self
                .execute_tool_calls(
                    &mut recorder,
                    &completion.tool_calls,
                    &cancellation,
                    true,
                    Some(Steering {
                        controls: &controls,
                        messages: &mut messages,
                    }),
                )
                .await?
            {
                ToolRound::Settled {
                    record,
                    user_cancelled,
                    injections,
                } => {
                    record.write(&mut messages, &completion.tool_calls);
                    (user_cancelled, injections)
                }
                ToolRound::Failed(message) => {
                    recorder.emit(EngineEvent::Lifecycle {
                        state: LifecycleState::Failed,
                        message: Some(message.clone()),
                    })?;
                    persist(&self.sink, &mut messages, recorder.state()).await?;
                    return Err(EngineError::ToolFailure(message));
                }
                ToolRound::Refused { message, record } => {
                    record.write(&mut messages, &completion.tool_calls);
                    recorder.emit(EngineEvent::Lifecycle {
                        state: LifecycleState::Failed,
                        message: Some(message.clone()),
                    })?;
                    persist(&self.sink, &mut messages, recorder.state()).await?;
                    persist_stats(&self.sink, &ledger.session_stats(recorder.tool_calls())).await?;
                    return Err(EngineError::ToolFailure(message));
                }
            };
            persist(&self.sink, &mut messages, recorder.state()).await?;
            checkpoints = checkpoints.saturating_add(1);
            if cancellation.is_cancelled() {
                break TurnStopReason::Cancelled;
            }
            ledger.count_step();
            recorder.emit(EngineEvent::StepCompleted {
                turn_completing: !matches!(messages.last(), Some(ModelMessage::Tool { .. })),
            })?;
            // Reference `_drain_pending_injections`, at the end of the step.
            messages.extend(injections.into_iter().map(ModelMessage::injected_user));
            if user_cancelled {
                break TurnStopReason::Complete;
            }
        };

        recorder.emit(EngineEvent::Lifecycle {
            state: lifecycle_for(&stop_reason),
            message: Some(stop_message(&stop_reason).to_owned()),
        })?;
        persist(&self.sink, &mut messages, recorder.state()).await?;
        persist_stats(&self.sink, &ledger.session_stats(recorder.tool_calls())).await?;
        let (events, snapshot) = recorder.finish();
        Ok(TurnOutcome {
            session_id: snapshot.session_id.clone(),
            events,
            snapshot,
            messages,
            usage: ledger.usage,
            context_tokens: ledger.context_tokens,
            price_micros: ledger.price_micros,
            steps: ledger.steps,
            checkpoints,
            stop_reason,
        })
    }

    /// The model this request addresses.
    ///
    /// A turn's own override wins, and the provider answers for the session's
    /// active model otherwise. The trace span, the request event and the model
    /// call span all report this one answer, so a client never sees three
    /// spellings of the same request.
    pub(super) fn resolved_model(&self, input: &ProviderInput) -> Option<String> {
        input
            .model_override
            .clone()
            .or_else(|| self.provider.model().map(ToOwned::to_owned))
    }

    /// Appends the synthetic `skill` call pair when the message is a slash
    /// invocation, reproducing reference `_inject_invoked_skill`: the model
    /// reads the same conversation whether the operator or one of its own tool
    /// calls loaded the skill, and a repeat is acknowledged rather than
    /// rendered again.
    pub(super) fn inject_invoked_skill(
        &self,
        recorder: &mut TurnRecorder<'_>,
        messages: &mut Vec<ModelMessage>,
        content: &str,
    ) -> Result<(), EngineError> {
        let Some(resolver) = &self.settings.invoked_skills else {
            return Ok(());
        };
        let Some(invoked) = resolver.resolve(content) else {
            return Ok(());
        };
        let appended = crate::skills::append_invoked_skill(messages, &invoked);
        // Reference `_inject_invoked_skill`: the pair records the call's
        // presentation and the result it answered with.
        let working_directory = self.settings.working_directory.as_deref();
        let detail =
            EffectDetail::for_encoded_call_at("skill", &appended.arguments, working_directory);
        let typed = &appended.output.typed_result;
        let display = detail.completed_display(typed, &appended.output.display, working_directory);
        let presentation = crate::events::detail::result_presentation(
            detail.kind,
            &display,
            &appended.output.projected_result,
        );
        for message in messages.iter_mut().rev().take(2) {
            match message {
                ModelMessage::Assistant { tool_calls, .. } => {
                    for call in tool_calls.iter_mut() {
                        call.presentation = Some(detail.presentation());
                    }
                }
                ModelMessage::Tool { result, .. } if typed.is_object() => {
                    *result = Some(Box::new(PersistedToolResult {
                        output: typed.clone(),
                        duration: None,
                        cancelled: false,
                        presentation: Some(presentation.clone()),
                    }));
                }
                _ => {}
            }
        }
        recorder.emit(EngineEvent::ToolCall {
            call_id: appended.call_id.clone(),
            name: "skill".to_owned(),
            arguments: appended.arguments,
            remote: None,
        })?;
        recorder.emit(EngineEvent::ToolResult {
            call_id: appended.call_id,
            content: appended.output.model_text,
            typed_result: appended.output.typed_result,
            projected_result: appended.output.projected_result,
            display: appended.output.display,
            duration_ms: 0,
            is_error: false,
            cancelled: false,
            skipped: false,
            approval: EffectApproval::default(),
        })?;
        Ok(())
    }

    /// Reads every file the prompt mentions with `@`, as though the model had
    /// asked for it, before the first model call.
    ///
    /// Each distinct file runs through `read_file` like any other call, so its
    /// permission, its client delegation and its public effect are the tool's
    /// own. A folder or an image mention is not read, and a session without
    /// `read_file` reads nothing. The answer is the message of a call that
    /// failed the turn. Reference `_inject_mentioned_files`
    /// (`vibe/core/agent_loop/_loop.py`).
    pub(super) async fn inject_mentioned_files(
        &self,
        recorder: &mut TurnRecorder<'_>,
        messages: &mut Vec<ModelMessage>,
        content: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>, EngineError> {
        const READ_FILE: &str = "read_file";
        if !self.tools.publishes(READ_FILE) {
            return Ok(None);
        }
        let base = self
            .settings
            .working_directory
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let payload = crate::path_resources::build_path_prompt_payload(&base, content);
        for resource in payload
            .resources
            .iter()
            .filter(|resource| resource.kind == crate::path_resources::PathResourceKind::File)
        {
            let call = ModelToolCall {
                id: crate::session_id::uuid_v4(),
                name: READ_FILE.to_owned(),
                // The reference serializes the arguments with `json.dumps`,
                // whose separators carry a space.
                arguments: format!(
                    "{{\"file_path\": {}}}",
                    Value::String(resource.path.to_string_lossy().into_owned())
                ),
                presentation: None,
            };
            messages.push(ModelMessage::Assistant {
                message_id: None,
                reasoning_message_id: None,
                content: String::new(),
                reasoning: None,
                reasoning_payloads: Vec::new(),
                tool_calls: vec![call.clone()],
                keeps_empty_content: true,
            });
            let calls = [call];
            match self
                .execute_tool_calls(recorder, &calls, cancellation, false, None)
                .await?
            {
                ToolRound::Settled { record, .. } => record.write(messages, &calls),
                ToolRound::Failed(message) | ToolRound::Refused { message, .. } => {
                    return Ok(Some(message));
                }
            }
        }
        Ok(None)
    }

    /// Reference `_dispatch_post_turn_hooks`: the post-agent chain's events,
    /// published once it ends, and the message a denying hook sends the model
    /// back with.
    async fn run_post_agent_hooks(
        &self,
        recorder: &mut TurnRecorder<'_>,
    ) -> Result<Option<String>, EngineError> {
        let Some(hooks) = self.settings.hooks.as_ref() else {
            return Ok(None);
        };
        let session_id = recorder.state().session_id.clone();
        let mut events = Vec::new();
        let mut retry = None;
        hooks
            .manager
            .run(
                HookInvocation::PostAgent {
                    context: hooks.context(&session_id),
                },
                |item| {
                    match item {
                        HookYield::Event(event) => events.push(event),
                        HookYield::UserMessage(content) => retry = Some(content),
                        _ => {}
                    }
                    std::ops::ControlFlow::Continue(())
                },
            )
            .await;
        for event in events {
            recorder.emit(EngineEvent::Hook { event })?;
        }
        Ok(retry)
    }
}

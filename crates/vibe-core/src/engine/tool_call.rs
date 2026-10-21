//! One tool call, from its pre-tool hooks to its post-tool hooks.
//!
//! A call runs beside the other calls of its round and reports to the round
//! through [`CallSignal`]s, in the order it produced them, until it settles as
//! a [`CallSettled`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde_json::Value;
use tokio::sync::mpsc;

use super::{
    Compactor, CompletionProvider, ConversationEngine, ToolExecutor, ToolStreamSink,
    TranscriptSink, TurnHooks,
};
use crate::events::{
    EffectApproval, EffectDetail, EngineEvent, ModelToolCall, PersistedToolResult,
};
use crate::hooks::{HookInvocation, HookYield, ToolStatus};
use crate::text::bounded_utf8;
use crate::tools::{MAX_TOOL_ERROR_BYTES, ToolExecutionOutput};
use crate::tracing::{ToolSpan, set_tool_result, tool_span};

impl<P, T, C, S> ConversationEngine<P, T, C, S>
where
    P: CompletionProvider,
    T: ToolExecutor,
    C: Compactor,
    S: TranscriptSink,
{
    /// One call, from its pre-tool hooks to its post-tool hooks. Reference
    /// `_execute_tool_call`.
    ///
    /// Everything the call reports before it settles travels through
    /// `signals`, so its hook events and its result event reach the transcript
    /// in the order the call produced them while other calls run beside it.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_tool_call(
        &self,
        index: usize,
        call: &ModelToolCall,
        mut arguments: String,
        output: ToolStreamSink,
        signals: &mpsc::UnboundedSender<CallSignal>,
        stages: CallStages,
        session_id: &str,
    ) -> CallSettled {
        let emit = |event: EngineEvent| {
            let _ = signals.send(CallSignal::Event(event));
        };
        let hooks = self.settings.hooks.as_ref();
        let mut rewritten = None;
        // Reference `_serialize_tool_input`: the validated arguments, with
        // every default filled in. A call whose arguments do not validate
        // fails before any hook sees it, as the reference fails it while
        // resolving the call.
        let mut tool_input =
            hooks.and_then(|_| self.tools.prepare_arguments(&call.name, &arguments).ok());
        if let (Some(hooks), Some(validated)) = (hooks, tool_input.clone()) {
            let pipeline = self
                .run_pre_tool_hooks(hooks, call, validated, session_id)
                .await;
            // Reference `_run_pre_tool_pipeline` buffers the chain's events
            // and yields them before the denial or the call.
            for event in pipeline.events {
                emit(EngineEvent::Hook { event });
            }
            if let Some((hook_name, content)) = pipeline.denial {
                let denial = format!(
                    "<tool_error>Tool '{}' was denied by hook '{hook_name}': {content}</tool_error>",
                    call.name
                );
                emit(EngineEvent::ToolResult {
                    call_id: call.id.clone(),
                    content: denial.clone(),
                    typed_result: Value::Null,
                    projected_result: Value::Null,
                    display: Value::Null,
                    duration_ms: 0,
                    is_error: false,
                    cancelled: false,
                    skipped: true,
                    approval: EffectApproval::default(),
                });
                let _ = signals.send(CallSignal::Settled {
                    index,
                    answer: Answer::text(denial.clone(), false),
                });
                return CallSettled::Answered {
                    answer: Answer::text(denial, false),
                    user_cancelled: false,
                    arguments: None,
                    injection: None,
                };
            }
            if pipeline.rewritten {
                arguments =
                    crate::pycompat::dumps(&pipeline.tool_input, crate::pycompat::Dumps::DEFAULT);
                rewritten = Some(arguments.clone());
            }
            tool_input = Some(pipeline.tool_input);
        }
        if let Some(tool_input) = &tool_input {
            let _ = signals.send(CallSignal::Prepared {
                index,
                tool_input: tool_input.clone(),
            });
        }
        stages.past_pre_tool.store(true, Ordering::SeqCst);
        let started_at = Instant::now();
        let result = crate::tools::track_call_start(
            Arc::clone(&stages.progress),
            // Reference `_loop.py` opens `tool_span` around the execution
            // itself, so a tool that streams for a minute is one span rather
            // than a point in the parent's timeline.
            tool_span(
                ToolSpan {
                    tool_name: &call.name,
                    call_id: &call.id,
                    arguments: &arguments,
                },
                async {
                    let result = self
                        .tools
                        .execute_call(&call.id, &call.name, &arguments, output)
                        .await;
                    if let Ok(output) = &result {
                        set_tool_result(&output.model_text);
                    }
                    result
                },
            ),
        )
        .await;
        let elapsed = started_at.elapsed();
        // What the post-tool hooks are shown, when the call ran: reference
        // `_invoke_tool` and the failure branch of `_execute_tool_call`.
        let mut ran = None;
        let (output, is_error) = match result {
            Ok(output) if output.turn_failure.is_some() => {
                return CallSettled::TurnFailure(output.turn_failure.unwrap_or_default());
            }
            Ok(output) if output.failure.is_some() => {
                let message = output.failure.unwrap_or_default();
                // Reference `_execute_tool_call`: the model reads the error
                // under the tool's name and the error tag, a post-tool hook
                // reads it bare.
                let mut failed = ToolExecutionOutput::text(format!(
                    "<tool_error>{} failed: {}</tool_error>",
                    call.name,
                    bounded_utf8(&message, MAX_TOOL_ERROR_BYTES, "…")
                ));
                failed.approval = output.approval;
                ran = Some((ToolStatus::Failure, None, Some(message)));
                (failed, true)
            }
            Ok(output) => {
                if output.skip.is_none() {
                    let cancelled =
                        output.typed_result.get("cancelled") == Some(&Value::Bool(true));
                    let status = if cancelled {
                        ToolStatus::Cancelled
                    } else {
                        ToolStatus::Success
                    };
                    let typed = output
                        .typed_result
                        .is_object()
                        .then(|| output.typed_result.clone());
                    ran = Some((status, typed, None));
                }
                (output, false)
            }
            Err(message) => (
                ToolExecutionOutput::text(bounded_utf8(&message, MAX_TOOL_ERROR_BYTES, "…")),
                true,
            ),
        };
        let user_cancelled = output.skip.is_some_and(|skip| skip.cancelled);
        let injection = output.pending_injection.clone();
        // Reference `_run_post_tool_and_finalize`: an answer the tool returned
        // is persisted with its typed result, how long it ran and how it was
        // presented.
        let persisted = match &ran {
            Some((status, Some(typed), None)) => {
                let detail = match self.tools.remote_origin(&call.name) {
                    Some(remote) => EffectDetail::for_proxied_call(&call.name, &arguments, &remote),
                    None => EffectDetail::for_encoded_call_at(
                        &call.name,
                        &arguments,
                        self.settings.working_directory.as_deref(),
                    ),
                };
                let display = detail.completed_display(
                    typed,
                    &output.display,
                    self.settings.working_directory.as_deref(),
                );
                Some(Box::new(PersistedToolResult {
                    output: typed.clone(),
                    duration: serde_json::Number::from_f64(elapsed.as_secs_f64()),
                    cancelled: *status == ToolStatus::Cancelled,
                    presentation: Some(crate::events::detail::result_presentation(
                        detail.kind,
                        &display,
                        &output.projected_result,
                    )),
                }))
            }
            _ => None,
        };
        // Reference `ToolResultEvent.duration` keeps fractions of a
        // millisecond, so a call that answered never reports taking none.
        // A call that failed or never ran reports none, as the reference's
        // failure and skip events carry no duration.
        let duration_ms = if ran.is_some() && !is_error {
            u64::try_from(elapsed.as_micros().div_ceil(1_000)).unwrap_or(u64::MAX)
        } else {
            0
        };
        emit(EngineEvent::ToolResult {
            call_id: call.id.clone(),
            content: output.model_text.clone(),
            typed_result: output.typed_result,
            projected_result: output.projected_result,
            display: output.display,
            duration_ms,
            is_error,
            cancelled: user_cancelled,
            skipped: output.skip.is_some(),
            approval: output.approval.into(),
        });
        let mut content = output.model_text;
        let _ = signals.send(CallSignal::Settled {
            index,
            answer: Answer {
                content: content.clone(),
                is_error,
                result: persisted.clone(),
            },
        });
        if let (Some(hooks), Some(tool_input), Some((tool_status, tool_output, tool_error))) =
            (hooks, tool_input, ran)
        {
            // Reference `_run_post_tool_and_finalize`: the events stream as
            // the chain runs, and the last replacement is what the model reads.
            hooks
                .manager
                .run(
                    HookInvocation::PostTool {
                        context: hooks.context(session_id),
                        tool_name: call.name.clone(),
                        tool_call_id: call.id.clone(),
                        tool_input,
                        tool_status,
                        tool_output,
                        tool_output_text: content.clone(),
                        // Reference `PostToolFinalization.duration_ms` is
                        // left at zero for a call that raised.
                        duration_ms: if tool_status == ToolStatus::Failure {
                            0.0
                        } else {
                            elapsed.as_secs_f64() * 1000.0
                        },
                        tool_error,
                    },
                    |item| {
                        match item {
                            HookYield::Event(event) => emit(EngineEvent::Hook { event }),
                            HookYield::TextReplacement(text) => content = text,
                            _ => {}
                        }
                        std::ops::ControlFlow::Continue(())
                    },
                )
                .await;
        }
        CallSettled::Answered {
            answer: Answer {
                content,
                is_error,
                result: persisted,
            },
            user_cancelled,
            arguments: rewritten,
            injection,
        }
    }

    /// Reference `_run_pre_tool_pipeline`: each rewrite is validated against
    /// the tool's arguments as it arrives, and the first one that fails ends
    /// the chain as a denial.
    async fn run_pre_tool_hooks(
        &self,
        hooks: &TurnHooks,
        call: &ModelToolCall,
        tool_input: Value,
        session_id: &str,
    ) -> PreToolPipeline {
        let mut pipeline = PreToolPipeline {
            events: Vec::new(),
            denial: None,
            tool_input: tool_input.clone(),
            rewritten: false,
        };
        hooks
            .manager
            .run(
                HookInvocation::PreTool {
                    context: hooks.context(session_id),
                    tool_name: call.name.clone(),
                    tool_call_id: call.id.clone(),
                    tool_input,
                },
                |item| match item {
                    HookYield::Event(event) => {
                        pipeline.events.push(event);
                        std::ops::ControlFlow::Continue(())
                    }
                    HookYield::ToolDenial { hook_name, content } => {
                        pipeline.denial = Some((hook_name, content));
                        std::ops::ControlFlow::Break(())
                    }
                    HookYield::ToolInputRewrite {
                        hook_name,
                        tool_input,
                    } => match self
                        .tools
                        .prepare_arguments(&call.name, &tool_input.to_string())
                    {
                        Ok(validated) => {
                            pipeline.tool_input = validated;
                            pipeline.rewritten = true;
                            std::ops::ControlFlow::Continue(())
                        }
                        // Reference `_apply_tool_input_rewrite`.
                        Err(error) => {
                            pipeline.denial = Some((
                                hook_name.clone(),
                                format!(
                                    "Hook '{hook_name}' rewrote tool_input but the result failed validation against {}: {error}",
                                    call.name
                                ),
                            ));
                            std::ops::ControlFlow::Break(())
                        }
                    },
                    _ => std::ops::ControlFlow::Continue(()),
                },
            )
            .await;
        pipeline
    }
}

/// What a pre-tool chain left the call to do.
struct PreToolPipeline {
    events: Vec<crate::hooks::HookEvent>,
    /// The hook that denied the call and why.
    denial: Option<(String, String)>,
    /// The arguments the call runs with, rewritten or not.
    tool_input: Value,
    rewritten: bool,
}

/// What a running call reports before it settles.
pub(super) enum CallSignal {
    Event(EngineEvent),
    /// The validated arguments a cancelled call is shown to its post-tool
    /// hooks with.
    Prepared {
        index: usize,
        tool_input: Value,
    },
    /// The answer as the result event published it, which stands if the turn
    /// is cancelled while its post-tool hooks run.
    Settled {
        index: usize,
        answer: Answer,
    },
}

/// What one call left behind to track it as it runs.
#[derive(Clone)]
pub(super) struct CallStages {
    /// What the call reached at its permission gate.
    pub(super) progress: Arc<crate::tools::CallProgress>,
    /// Whether the call is past its pre-tool hooks.
    pub(super) past_pre_tool: Arc<AtomicBool>,
}

/// One call's answer as the transcript records it.
#[derive(Debug)]
pub(super) struct Answer {
    pub(super) content: String,
    pub(super) is_error: bool,
    pub(super) result: Option<Box<PersistedToolResult>>,
}

impl Answer {
    pub(super) fn text(content: String, is_error: bool) -> Self {
        Self {
            content,
            is_error,
            result: None,
        }
    }
}

/// How one call settled.
pub(super) enum CallSettled {
    Answered {
        answer: Answer,
        user_cancelled: bool,
        /// The arguments a hook rewrote the call to, as the assistant message
        /// is patched to carry them.
        arguments: Option<String>,
        /// The message the call left for the end of the step.
        injection: Option<String>,
    },
    /// The call's answer failed the turn.
    TurnFailure(String),
}

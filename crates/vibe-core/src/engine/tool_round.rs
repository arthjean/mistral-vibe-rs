//! One round of tool calls: every call a reply declared, run concurrently.
//!
//! The calls that fail to resolve settle first, then the rest run side by side
//! and settle in completion order, which is the order reference
//! `_handle_tool_response` appends their answers in. Each call's state lives in
//! one [`CallSlot`] from the moment it is resolved until the transcript records
//! its answer, and what the round leaves the transcript to record is one
//! [`RoundRecord`].

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::{FutureExt, StreamExt, stream::FuturesUnordered};
use serde_json::Value;
use tokio::sync::mpsc;

use super::controls::{CancellationToken, Steering, steer_arrived};
use super::ledger::TurnRecorder;
use super::tool_call::{Answer, CallSettled, CallSignal, CallStages};
use super::{
    Compactor, CompletionProvider, ConversationEngine, EngineError, INTERRUPTED_TOOL_RESULT,
    ToolExecutor, ToolStreamItem, ToolStreamSink, TranscriptSink,
};
use crate::events::{EffectApproval, EffectDetail, EngineEvent, ModelMessage, ModelToolCall};
use crate::hooks::{HookInvocation, HookYield, ToolStatus};
use crate::tools::ToolVerdict;

/// Buffered tool output chunks awaiting projection.
const TOOL_STREAM_CAPACITY: usize = 256;

impl<P, T, C, S> ConversationEngine<P, T, C, S>
where
    P: CompletionProvider,
    T: ToolExecutor,
    C: Compactor,
    S: TranscriptSink,
{
    /// Runs every declared tool call concurrently.
    ///
    /// Events follow completion order so observers see progress as it happens,
    /// and so does the transcript: reference `_handle_tool_response` appends
    /// each answer as its call settles, after the calls that failed to resolve,
    /// which settle first. A cancelled call that had passed its pre-tool hooks
    /// is answered as interrupted; one cancelled before leaves its call
    /// unanswered, which the next turn fills in
    /// ([`crate::events::fill_missing_tool_responses`]).
    pub(super) async fn execute_tool_calls(
        &self,
        recorder: &mut TurnRecorder<'_>,
        tool_calls: &[ModelToolCall],
        cancellation: &CancellationToken,
        announce: bool,
        mut steering: Option<Steering<'_>>,
    ) -> Result<ToolRound, EngineError> {
        let mut slots = tool_calls
            .iter()
            .map(|call| CallSlot::new(call, announce, self.tools.publishes(&call.name)))
            .collect::<Vec<_>>();
        // Reference `_build_tool_call_events`: each call is announced by name
        // while the response streams, before its arguments are validated.
        for slot in slots.iter().filter(|slot| slot.announced) {
            recorder.emit(EngineEvent::ToolCallAnnounced {
                call_id: slot.call.id.clone(),
                name: slot.call.name.clone(),
                remote: self.tools.remote_origin(&slot.call.name),
            })?;
        }
        // Reference `_emit_failed_tool_events`: a call naming no available
        // tool, or whose arguments do not validate, settles first, as an error
        // the model reads, and never runs.
        let mut order = Vec::with_capacity(slots.len());
        for (index, slot) in slots.iter_mut().enumerate() {
            let failure = if slot.resolved {
                match self
                    .tools
                    .prepare_arguments(&slot.call.name, &slot.arguments)
                {
                    Ok(_) => None,
                    // Reference `resolve_tool_calls` fails the call as
                    // `Invalid arguments: <validation error>`.
                    Err(error) => Some(format!("Invalid arguments: {error}")),
                }
            } else {
                // Reference `resolve_tool_calls` fails the call as
                // `Unknown tool '<name>'`.
                Some(format!("Unknown tool '{}'", slot.call.name))
            };
            let Some(failure) = failure else {
                slot.runnable = true;
                continue;
            };
            let error = format!("<tool_error>{}: {failure}</tool_error>", slot.call.name);
            if !slot.announced {
                recorder.emit(EngineEvent::ToolCallUnresolved {
                    call_id: slot.call.id.clone(),
                    name: slot.call.name.clone(),
                })?;
            }
            recorder.emit(EngineEvent::ToolResult {
                call_id: slot.call.id.clone(),
                content: error.clone(),
                typed_result: Value::Null,
                projected_result: Value::Null,
                display: Value::Null,
                duration_ms: 0,
                is_error: true,
                cancelled: false,
                skipped: false,
                approval: EffectApproval::default(),
            })?;
            slot.answer = Some(Answer::text(error, true));
            order.push(index);
        }
        for slot in slots.iter_mut().filter(|slot| slot.runnable) {
            let remote = self.tools.remote_origin(&slot.call.name);
            // Reference `_record_tool_call_presentation`: the call keeps the
            // presentation its event carried.
            let detail = match &remote {
                Some(remote) => {
                    EffectDetail::for_proxied_call(&slot.call.name, &slot.arguments, remote)
                }
                None => EffectDetail::for_encoded_call_at(
                    &slot.call.name,
                    &slot.arguments,
                    self.settings.working_directory.as_deref(),
                ),
            };
            slot.presentation = Some(detail.presentation());
            recorder.emit(EngineEvent::ToolCall {
                call_id: slot.call.id.clone(),
                name: slot.call.name.clone(),
                arguments: slot.arguments.clone(),
                // The registry is asked once, here, so the projection presents
                // the call without reaching back into a catalog it does not
                // hold.
                remote,
            })?;
        }
        let session_id = recorder.state().session_id.clone();
        let mut pending = FuturesUnordered::new();
        let (stream_tx, mut stream_rx) = mpsc::channel(TOOL_STREAM_CAPACITY);
        // What a call reports before it settles: its hook events and its
        // result event, in the order it produced them.
        let (signal_tx, mut signal_rx) = mpsc::unbounded_channel::<CallSignal>();
        for (index, slot) in slots.iter().enumerate().filter(|(_, slot)| slot.runnable) {
            let sender = stream_tx.clone();
            let output: ToolStreamSink = Arc::new(move |chunk| {
                sender
                    .try_send((index, chunk))
                    .map_err(|error| format!("tool output backpressure: {error}"))
            });
            let signals = signal_tx.clone();
            let call = slot.call;
            let arguments = slot.arguments.clone();
            let stages = slot.stages.clone();
            let session_id = session_id.clone();
            pending.push(
                async move {
                    let settled = self
                        .run_tool_call(
                            index,
                            call,
                            arguments,
                            output,
                            &signals,
                            stages,
                            &session_id,
                        )
                        .await;
                    (index, settled)
                }
                .boxed(),
            );
        }
        drop(stream_tx);
        drop(signal_tx);

        // Reference `is_user_cancellation_event`: a call the user declined
        // without a reason ends the turn once this round settles.
        let mut user_cancelled = false;
        let mut failure = None;
        // Each steer taken while the calls run, with how many answers had
        // settled before it and how many messages it appended.
        let mut steered = Vec::new();
        let mut injections = Vec::new();
        while !pending.is_empty() {
            tokio::select! {
                () = steer_arrived(steering.as_ref()) => {
                    let Some(steering) = steering.as_mut() else {
                        continue;
                    };
                    for (content, inject_implicit) in steering.controls.take_steers()? {
                        let before = steering.messages.len();
                        if let Some(message) = self
                            .apply_steer(recorder, steering.messages, content, inject_implicit, cancellation)
                            .await?
                        {
                            failure = Some((None, message));
                            break;
                        }
                        steered.push((order.len(), steering.messages.len() - before));
                    }
                    if failure.is_some() {
                        break;
                    }
                }
                streamed = stream_rx.recv() => {
                    if let Some((index, item)) = streamed {
                        recorder.emit(stream_event(&tool_calls[index].id, item))?;
                    }
                }
                signal = signal_rx.recv() => {
                    if let Some(signal) = signal {
                        drain_tool_stream(recorder, tool_calls, &mut stream_rx)?;
                        apply_signal(recorder, signal, &mut slots)?;
                    }
                }
                next = pending.next() => {
                    let Some((index, settled)) = next else {
                        break;
                    };
                    drain_tool_stream(recorder, tool_calls, &mut stream_rx)?;
                    while let Ok(signal) = signal_rx.try_recv() {
                        apply_signal(recorder, signal, &mut slots)?;
                    }
                    match settled {
                        CallSettled::TurnFailure(message) => {
                            failure = Some((Some(index), message));
                            break;
                        }
                        CallSettled::Answered {
                            answer,
                            user_cancelled: cancelled,
                            arguments,
                            injection,
                        } => {
                            injections.extend(injection);
                            user_cancelled |= cancelled;
                            slots[index].answer = Some(answer);
                            slots[index].patched = arguments;
                            order.push(index);
                        }
                    }
                }
                () = cancellation.cancelled() => break,
            }
        }
        drop(pending);
        drain_tool_stream(recorder, tool_calls, &mut stream_rx)?;
        while let Ok(signal) = signal_rx.try_recv() {
            apply_signal(recorder, signal, &mut slots)?;
        }
        match failure {
            // Reference `_execute_tool_call` answers a refused call as a
            // failure of the tool, which the transcript keeps, before the
            // refusal fails the turn.
            Some((Some(index), message)) => {
                let slot = &mut slots[index];
                let content = self
                    .abandon_tool_call(
                        recorder,
                        slot.call,
                        slot.prepared_input.take(),
                        &session_id,
                        &message,
                    )
                    .await?;
                slot.answer = Some(Answer::text(content, true));
                order.push(index);
                return Ok(ToolRound::Refused {
                    message,
                    record: RoundRecord::from_slots(&mut slots, order, steered),
                });
            }
            Some((None, message)) => return Ok(ToolRound::Failed(message)),
            None => {}
        }

        // A call whose result was published but whose post-tool hooks were
        // cut short keeps the answer it published.
        for (index, slot) in slots.iter().enumerate() {
            if slot.answer.is_some() && !order.contains(&index) {
                order.push(index);
            }
        }
        for (index, slot) in slots.iter_mut().enumerate() {
            if slot.answer.is_some() || !slot.runnable {
                continue;
            }
            // Reference `_execute_tool_call`: a call cancelled inside its
            // pre-tool pipeline raises before any answer is written.
            if !slot.stages.past_pre_tool.load(Ordering::SeqCst) {
                continue;
            }
            if slot.stages.progress.awaiting_approval() {
                let content = self
                    .abandon_tool_call(
                        recorder,
                        slot.call,
                        slot.prepared_input.take(),
                        &session_id,
                        TURN_ENDED,
                    )
                    .await?;
                slot.answer = Some(Answer::text(content, true));
                order.push(index);
                continue;
            }
            let decision = slot.stages.progress.approval();
            recorder.emit(EngineEvent::ToolResult {
                call_id: slot.call.id.clone(),
                content: INTERRUPTED_TOOL_RESULT.to_owned(),
                typed_result: Value::Null,
                projected_result: Value::Null,
                display: Value::Null,
                duration_ms: 0,
                is_error: true,
                cancelled: true,
                skipped: false,
                // Reference `_execute_tool_call`: the decision the gate reached,
                // or `skip` when the call was cancelled before it decided.
                approval: EffectApproval {
                    decision: Some(
                        decision.map_or(ToolVerdict::Skip, |approval| approval.decision),
                    ),
                    approval_type: decision.map(|approval| approval.approval_type),
                    approval_source: decision.map(|approval| approval.approval_source),
                },
            })?;
            let mut content = INTERRUPTED_TOOL_RESULT.to_owned();
            // Reference `_finalize_cancelled_tool`: a call whose body started
            // is still shown to the post-tool hooks, shielded from the
            // cancellation, and one cancelled before it ran is not.
            if let (Some(hooks), Some(tool_input)) = (
                self.settings.hooks.as_ref(),
                slot.prepared_input
                    .take()
                    .filter(|_| slot.stages.progress.started()),
            ) {
                let call = slot.call;
                // Only the text survives: the reference yields the chain's
                // events from a generator its interrupted consumer has left,
                // so no client ever reads them.
                hooks
                    .manager
                    .run(
                        HookInvocation::PostTool {
                            context: hooks.context(&session_id),
                            tool_name: call.name.clone(),
                            tool_call_id: call.id.clone(),
                            tool_input,
                            tool_status: ToolStatus::Cancelled,
                            tool_output: None,
                            tool_output_text: content.clone(),
                            tool_error: Some(content.clone()),
                            duration_ms: 0.0,
                        },
                        |item| {
                            if let HookYield::TextReplacement(text) = item {
                                content = text;
                            }
                            std::ops::ControlFlow::Continue(())
                        },
                    )
                    .await;
            }
            slot.answer = Some(Answer::text(content, true));
            order.push(index);
        }
        Ok(ToolRound::Settled {
            record: RoundRecord::from_slots(&mut slots, order, steered),
            user_cancelled,
            injections,
        })
    }

    /// A call whose pending answer was rejected with `rejection`: still
    /// waiting on its approval when the turn ended, where reference
    /// `_finalize_turn` rejects the request with `CallbackRejectedError`
    /// ("Turn ended"), or refused by an answer that fails the turn. Reference
    /// `_execute_tool_call` answers either as a failure of the tool and shows
    /// it to the post-tool hooks.
    async fn abandon_tool_call(
        &self,
        recorder: &mut TurnRecorder<'_>,
        call: &ModelToolCall,
        tool_input: Option<Value>,
        session_id: &str,
        rejection: &str,
    ) -> Result<String, EngineError> {
        recorder.emit(EngineEvent::ToolCallAbandoned {
            call_id: call.id.clone(),
        })?;
        let mut content = format!("<tool_error>{} failed: {rejection}</tool_error>", call.name);
        if let (Some(hooks), Some(tool_input)) = (self.settings.hooks.as_ref(), tool_input) {
            // Only the text survives, as for an interrupted call.
            hooks
                .manager
                .run(
                    HookInvocation::PostTool {
                        context: hooks.context(session_id),
                        tool_name: call.name.clone(),
                        tool_call_id: call.id.clone(),
                        tool_input,
                        tool_status: ToolStatus::Failure,
                        tool_output: None,
                        tool_output_text: content.clone(),
                        tool_error: Some(rejection.to_owned()),
                        duration_ms: 0.0,
                    },
                    |item| {
                        if let HookYield::TextReplacement(text) = item {
                            content = text;
                        }
                        std::ops::ControlFlow::Continue(())
                    },
                )
                .await;
        }
        Ok(content)
    }
}

/// What reference `_finalize_turn` rejects a call still waiting on its gate
/// with.
const TURN_ENDED: &str = "Turn ended";

fn apply_signal(
    recorder: &mut TurnRecorder<'_>,
    signal: CallSignal,
    slots: &mut [CallSlot<'_>],
) -> Result<(), EngineError> {
    match signal {
        CallSignal::Event(event) => recorder.emit(event),
        CallSignal::Prepared { index, tool_input } => {
            if let Some(slot) = slots.get_mut(index) {
                slot.prepared_input = Some(tool_input);
            }
            Ok(())
        }
        CallSignal::Settled { index, answer } => {
            if let Some(slot) = slots.get_mut(index) {
                slot.answer = Some(answer);
            }
            Ok(())
        }
    }
}

/// One call of a round, from the moment it is resolved until the transcript
/// records its answer.
struct CallSlot<'a> {
    call: &'a ModelToolCall,
    /// The arguments the call runs with. Reference `resolve_tool_calls` reads
    /// arguments that are not JSON as an empty object.
    arguments: String,
    /// Whether the call names a tool the session publishes.
    resolved: bool,
    /// Whether the call was announced while the reply streamed.
    announced: bool,
    /// Whether the call resolved and its arguments validated, so it runs.
    runnable: bool,
    /// The answer, once the call's result is published.
    answer: Option<Answer>,
    /// The arguments a hook rewrote the call to.
    patched: Option<String>,
    /// The presentation the call's event carried.
    presentation: Option<Value>,
    /// The validated arguments the call's post-tool hooks are shown, should
    /// the turn end before the call settles.
    prepared_input: Option<Value>,
    stages: CallStages,
}

impl<'a> CallSlot<'a> {
    fn new(call: &'a ModelToolCall, announce: bool, resolved: bool) -> Self {
        let arguments = if serde_json::from_str::<Value>(&call.arguments).is_ok() {
            call.arguments.clone()
        } else {
            "{}".to_owned()
        };
        Self {
            call,
            arguments,
            resolved,
            announced: announce && resolved,
            runnable: false,
            answer: None,
            patched: None,
            presentation: None,
            prepared_input: None,
            stages: CallStages {
                progress: Arc::new(crate::tools::CallProgress::default()),
                past_pre_tool: Arc::new(AtomicBool::new(false)),
            },
        }
    }
}

/// How one round of tool calls ended.
pub(super) enum ToolRound {
    /// The calls answered, and whether the user's decline of one of them ends
    /// the turn.
    Settled {
        record: RoundRecord,
        user_cancelled: bool,
        /// What the calls left for the end of the step, in the order they
        /// settled.
        injections: Vec<String>,
    },
    /// A steer failed the turn.
    Failed(String),
    /// A call's answer was refused, which fails the turn: the refusal, and
    /// the answers settled by then, the refused call's failure among them.
    Refused {
        message: String,
        record: RoundRecord,
    },
}

/// What a round leaves the transcript to record.
pub(super) struct RoundRecord {
    /// The calls answered, each with its index, in the order the transcript
    /// records them.
    answers: Vec<(usize, Answer)>,
    /// The arguments a hook rewrote each call to.
    patched: Vec<Option<String>>,
    /// The presentation each call that ran was given.
    presentations: Vec<Option<Value>>,
    /// The steers taken while the calls ran, already appended to the
    /// transcript: how many answers settled before each, and how many
    /// messages it appended.
    steered: Vec<(usize, usize)>,
}

impl RoundRecord {
    fn from_slots(
        slots: &mut [CallSlot<'_>],
        order: Vec<usize>,
        steered: Vec<(usize, usize)>,
    ) -> Self {
        Self {
            answers: order
                .into_iter()
                .filter_map(|index| slots[index].answer.take().map(|answer| (index, answer)))
                .collect(),
            patched: slots.iter_mut().map(|slot| slot.patched.take()).collect(),
            presentations: slots
                .iter_mut()
                .map(|slot| slot.presentation.take())
                .collect(),
            steered,
        }
    }

    /// Writes the round into `messages`: the assistant message that asked for
    /// `calls` carries what they ran with and how they were presented, and
    /// each answer follows as a tool message.
    pub(super) fn write(self, messages: &mut Vec<ModelMessage>, calls: &[ModelToolCall]) {
        patch_tool_call_arguments(messages, calls, &self.patched);
        record_tool_call_presentations(messages, calls, self.presentations);
        push_answers_around(messages, calls, self.answers, self.steered);
    }
}

/// Appends each call's answer as a tool message, in the order given, around
/// the steers the round took: the reference appends a steer when it arrives
/// and an answer when its call settles, so a steer sits after the answers
/// that settled before it. The steers are the last messages of `messages`.
fn push_answers_around(
    messages: &mut Vec<ModelMessage>,
    calls: &[ModelToolCall],
    answers: Vec<(usize, Answer)>,
    steered: Vec<(usize, usize)>,
) {
    let appended = steered.iter().map(|(_, count)| count).sum::<usize>();
    let mut tail = messages
        .split_off(messages.len().saturating_sub(appended))
        .into_iter();
    let mut steers = steered
        .into_iter()
        .map(|(settled, count)| (settled, tail.by_ref().take(count).collect::<Vec<_>>()))
        .collect::<VecDeque<_>>();
    for (position, (index, answer)) in answers.into_iter().enumerate() {
        while steers
            .front()
            .is_some_and(|(settled, _)| *settled <= position)
        {
            if let Some((_, steer)) = steers.pop_front() {
                messages.extend(steer);
            }
        }
        let Some(call) = calls.get(index) else {
            continue;
        };
        messages.push(ModelMessage::Tool {
            call_id: call.id.clone(),
            content: answer.content,
            is_error: answer.is_error,
            name: call.name.clone(),
            result: answer.result,
        });
    }
    for (_, steer) in steers {
        messages.extend(steer);
    }
}

/// Reference `_record_tool_call_presentation`: the assistant message that
/// asked for a call keeps the presentation the call ran under.
fn record_tool_call_presentations(
    messages: &mut [ModelMessage],
    calls: &[ModelToolCall],
    presentations: Vec<Option<Value>>,
) {
    for (call, presentation) in calls.iter().zip(presentations) {
        let Some(presentation) = presentation else {
            continue;
        };
        for message in messages.iter_mut().rev() {
            if let ModelMessage::Assistant { tool_calls, .. } = message
                && let Some(recorded) = tool_calls
                    .iter_mut()
                    .find(|recorded| recorded.id == call.id)
            {
                recorded.presentation = Some(presentation);
                break;
            }
        }
    }
}

/// Reference `_patch_assistant_tool_call_args`: the assistant message that
/// asked for a call a hook rewrote carries the arguments it ran with, so the
/// transcript and the next request show what actually ran.
fn patch_tool_call_arguments(
    messages: &mut [ModelMessage],
    calls: &[ModelToolCall],
    patched: &[Option<String>],
) {
    for (call, arguments) in calls.iter().zip(patched) {
        let Some(arguments) = arguments else {
            continue;
        };
        if call.id.is_empty() {
            continue;
        }
        'messages: for message in messages.iter_mut().rev() {
            if let ModelMessage::Assistant { tool_calls, .. } = message {
                for tool_call in tool_calls.iter_mut() {
                    if tool_call.id == call.id {
                        tool_call.arguments.clone_from(arguments);
                        break 'messages;
                    }
                }
            }
        }
    }
}

/// Projects every chunk a tool emitted before its result arrived.
fn drain_tool_stream(
    recorder: &mut TurnRecorder<'_>,
    tool_calls: &[ModelToolCall],
    stream_rx: &mut tokio::sync::mpsc::Receiver<(usize, ToolStreamItem)>,
) -> Result<(), EngineError> {
    while let Ok((index, item)) = stream_rx.try_recv() {
        recorder.emit(stream_event(&tool_calls[index].id, item))?;
    }
    Ok(())
}

/// The event one streamed item of the call `call_id` publishes.
fn stream_event(call_id: &str, item: ToolStreamItem) -> EngineEvent {
    match item {
        ToolStreamItem::Output(chunk) => EngineEvent::ToolStream {
            call_id: call_id.to_owned(),
            chunk,
        },
        ToolStreamItem::ChildSession(child_session_id) => EngineEvent::ToolChildSession {
            call_id: call_id.to_owned(),
            child_session_id,
        },
    }
}

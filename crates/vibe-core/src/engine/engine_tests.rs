//! One turn at a time, against scripted collaborators: what it publishes, what
//! it persists, and where it stops.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use serde_json::json;
use tokio::sync::Notify;

use super::*;
use crate::compaction::CompactionStatus;
use crate::events::{LifecycleState, ModelToolCall, ProjectionReducer, SessionHandoffCause};
use crate::llm::retry::{RetryObserver, RetryReason};
use crate::middleware::{ConversationContext, MiddlewareResult, ResetReason};
use crate::provider::{ProviderChunk, ProviderInput, RequestLimits};
use crate::storage::SessionStore;

#[derive(Default)]
struct ScriptedProvider {
    responses: Mutex<VecDeque<Result<AssistantMessage, ProviderError>>>,
    /// Every request's transcript, so a test can prove what the model was
    /// actually shown rather than what the turn ended up holding.
    requests: Mutex<Vec<Vec<ModelMessage>>>,
}

impl ScriptedProvider {
    fn new(responses: impl IntoIterator<Item = Result<AssistantMessage, ProviderError>>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl CompletionProvider for ScriptedProvider {
    fn complete<'a>(&'a self, input: &'a ProviderInput) -> ProviderFuture<'a> {
        Box::pin(async move {
            self.requests
                .lock()
                .map_err(|_| ProviderError::MalformedStream("fake lock poisoned".to_owned()))?
                .push(input.messages.clone());
            self.responses
                .lock()
                .map_err(|_| ProviderError::MalformedStream("fake lock poisoned".to_owned()))?
                .pop_front()
                .ok_or_else(|| ProviderError::MalformedStream("missing fake response".to_owned()))?
        })
    }
}

struct FakeTools;

impl ToolExecutor for FakeTools {
    fn execute<'a>(&'a self, name: &'a str, arguments: &'a str) -> ToolFuture<'a> {
        Box::pin(async move { Ok(ToolExecutionOutput::text(format!("{name}:{arguments}"))) })
    }
}

#[derive(Clone, Default)]
struct BlockingTools {
    started: Arc<Notify>,
}

impl ToolExecutor for BlockingTools {
    fn execute<'a>(&'a self, _name: &'a str, _arguments: &'a str) -> ToolFuture<'a> {
        Box::pin(async move {
            self.started.notify_one();
            std::future::pending::<Result<ToolExecutionOutput, String>>().await
        })
    }
}

#[derive(Clone, Default)]
struct ControlledTools {
    started_count: Arc<AtomicUsize>,
    all_started: Arc<Notify>,
    release_first: Arc<Notify>,
    release_second: Arc<Notify>,
}

impl ToolExecutor for ControlledTools {
    fn execute<'a>(&'a self, name: &'a str, _arguments: &'a str) -> ToolFuture<'a> {
        Box::pin(async move {
            if self.started_count.fetch_add(1, AtomicOrdering::SeqCst) == 1 {
                self.all_started.notify_one();
            }
            match name {
                "first" => self.release_first.notified().await,
                "second" => self.release_second.notified().await,
                _ => return Err("unexpected tool".to_owned()),
            }
            Ok(ToolExecutionOutput::new(format!("{name}-result"))
                .displayed_as(json!({"summary": name}))
                .typed(json!({"tool": name})))
        })
    }

    fn execute_stream<'a>(
        &'a self,
        name: &'a str,
        arguments: &'a str,
        output: ToolStreamSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            output(format!("{name}-chunk").into())?;
            self.execute(name, arguments).await
        })
    }
}

#[derive(Default)]
struct RecordingObserver {
    events: Mutex<Vec<EngineEvent>>,
    result_seen: Notify,
    stream_seen: Notify,
}

impl EventObserver for RecordingObserver {
    fn observe(&self, event: &EventEnvelope) -> Result<(), String> {
        if matches!(event.event, EngineEvent::ToolResult { .. }) {
            self.result_seen.notify_one();
        }
        if matches!(event.event, EngineEvent::ToolStream { .. }) {
            self.stream_seen.notify_one();
        }
        self.events
            .lock()
            .map_err(|_| "recording observer lock poisoned".to_owned())?
            .push(event.event.clone());
        Ok(())
    }
}

struct FakeCompactor;

impl Compactor for FakeCompactor {
    fn compact<'a>(
        &'a self,
        _current_session_id: &'a str,
        _messages: &'a [ModelMessage],
    ) -> CompactionFuture<'a> {
        Box::pin(async {
            Ok(CompactionResult {
                new_session_id: "session-2".to_owned(),
                summary: "compact summary".to_owned(),
                messages: vec![ModelMessage::System {
                    content: "summary".to_owned(),
                }],
                usage: Usage {
                    input_tokens: 30,
                    output_tokens: 5,
                    cached_tokens: 0,
                },
                failure: None,
            })
        })
    }

    fn cleared_session_id(&self, current_session_id: &str) -> Result<String, String> {
        Ok(format!("{current_session_id}-cleared"))
    }
}

#[derive(Clone, Default)]
struct BlockingProvider {
    started: Arc<Notify>,
}

impl CompletionProvider for BlockingProvider {
    fn complete<'a>(&'a self, _input: &'a ProviderInput) -> ProviderFuture<'a> {
        Box::pin(std::future::pending())
    }

    fn stream<'a>(&'a self, _input: &'a ProviderInput) -> ProviderStreamFuture<'a> {
        let started = Arc::clone(&self.started);
        Box::pin(async move {
            Ok(ProviderStream {
                correlation_id: None,
                chunks: Box::pin(futures_util::stream::once(async move {
                    started.notify_one();
                    std::future::pending::<Result<ProviderChunk, ProviderError>>().await
                })),
            })
        })
    }
}

#[derive(Clone, Default)]
struct BlockingCompactor {
    started: Arc<Notify>,
}

impl Compactor for BlockingCompactor {
    fn compact<'a>(
        &'a self,
        _current_session_id: &'a str,
        _messages: &'a [ModelMessage],
    ) -> CompactionFuture<'a> {
        Box::pin(async move {
            self.started.notify_one();
            std::future::pending::<Result<CompactionResult, CompactionFailure>>().await
        })
    }

    fn cleared_session_id(&self, current_session_id: &str) -> Result<String, String> {
        Ok(format!("{current_session_id}-cleared"))
    }
}

#[derive(Clone, Default)]
struct BlockingSink {
    calls: Arc<AtomicUsize>,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl TranscriptSink for BlockingSink {
    fn persist<'a>(
        &'a self,
        _messages: &'a [ModelMessage],
        _snapshot: &'a ProjectionSnapshot,
    ) -> PersistenceFuture<'a> {
        Box::pin(async move {
            if self.calls.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                self.started.notify_one();
                self.release.notified().await;
            }
            Ok(())
        })
    }
}

/// A provider that reports one retry before answering, the way a backend
/// waiting on a retryable status does.
struct RetryingProvider {
    inner: ScriptedProvider,
}

impl CompletionProvider for RetryingProvider {
    fn complete<'a>(&'a self, input: &'a ProviderInput) -> ProviderFuture<'a> {
        self.inner.complete(input)
    }

    fn stream_observed<'a>(
        &'a self,
        input: &'a ProviderInput,
        retries: &'a (dyn RetryObserver + 'a),
    ) -> ProviderStreamFuture<'a> {
        retries.retrying(&RetryReason::for_status(503));
        self.stream(input)
    }
}

/// A retry is a fact about the wait, so the turn reports it while it is
/// still waiting rather than folding it into the outcome.
#[tokio::test]
async fn a_retried_request_is_reported_as_a_turn_event() {
    let outcome = ConversationEngine::new(RetryingProvider {
        inner: ScriptedProvider::new([Ok(completion("answer", Vec::new()))]),
    })
    .run_turn(
        "session-1",
        provider_input(),
        "hello",
        CancellationToken::default(),
    )
    .await
    .expect("turn completes");
    assert_eq!(
        outcome
            .events
            .iter()
            .filter_map(|event| match &event.event {
                EngineEvent::Retrying { category, detail } => {
                    Some((category.as_str(), detail.as_str()))
                }
                _ => None,
            })
            .collect::<Vec<_>>(),
        [("server_error", "HTTP 503")]
    );
}

/// US-016: the turn is one agent span, every model call and tool call the
/// turn made hangs off it, the usage the aggregation read reaches the model
/// call span, and the conversation id the turn published reaches the tool
/// call. Reference `_loop.py` opens `agent_span` around the run loop and
/// `tool_span` around each execution.
#[tokio::test]
async fn a_turn_hangs_every_call_off_one_agent_span() {
    let _exclusive = crate::tracing::harness::exclusive();
    let harness = crate::tracing::harness::Harness::install();
    let provider = ScriptedProvider::new([
        Ok(completion(
            "",
            vec![ModelToolCall {
                id: "call-1".to_owned(),
                name: "read".to_owned(),
                arguments: "{}".to_owned(),
                presentation: None,
            }],
        )),
        Ok(completion("done", Vec::new())),
    ]);
    let mut input = provider_input();
    input.model_override = Some("model-1".to_owned());
    let outcome = ConversationEngine::new(provider)
        .with_tools(FakeTools)
        .run_turn("session-1", input, "hello", CancellationToken::default())
        .await
        .expect("turn completes");
    assert_eq!(outcome.stop_reason, TurnStopReason::Complete);
    let drained = harness.drain();
    drop(harness);
    // The provider is global, so a test running beside this one exports
    // into the same collector. This turn is one trace, and the trace is
    // what tells its spans apart from everyone else's.
    let trace = drained
        .iter()
        .find(|span| {
            span.name == "invoke_agent mistral-vibe"
                && span.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == "gen_ai.request.model"
                        && attribute.value.as_str() == "model-1"
                })
        })
        .map(|span| span.span_context.trace_id())
        .expect("the turn opened an agent span");
    let spans = drained
        .iter()
        .filter(|span| span.span_context.trace_id() == trace)
        .collect::<Vec<_>>();

    let agents = spans
        .iter()
        .filter(|span| span.name == "invoke_agent mistral-vibe")
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(agents.len(), 1, "one turn is one agent span");
    let parent = agents[0].span_context.span_id();
    let attribute = |span: &opentelemetry_sdk::trace::SpanData, key: &str| {
        span.attributes
            .iter()
            .find(|attribute| attribute.key.as_str() == key)
            .map(|attribute| attribute.value.to_string())
    };
    assert_eq!(
        attribute(agents[0], "gen_ai.request.model"),
        Some("model-1".to_owned())
    );

    let calls = spans
        .iter()
        .filter(|span| span.name.starts_with("chat "))
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2, "both cycles opened a model call span");
    for call in &calls {
        assert_eq!(
            call.parent_span_id, parent,
            "every model call is a descendant"
        );
        assert_eq!(
            attribute(call, "gen_ai.usage.input_tokens"),
            Some("2".to_owned()),
            "the usage the aggregation read reached the span it ran under"
        );
        assert_eq!(
            attribute(call, "gen_ai.response.finish_reasons"),
            Some("[\"stop\"]".to_owned())
        );
    }

    let tools = spans
        .iter()
        .filter(|span| span.name == "execute_tool read")
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].parent_span_id, parent);
    assert_eq!(
        attribute(tools[0], "gen_ai.conversation.id"),
        Some("session-1".to_owned()),
        "the tool call read the conversation id out of the turn's baggage"
    );
    assert_eq!(
        attribute(tools[0], "gen_ai.tool.call.id"),
        Some("call-1".to_owned())
    );
}

/// US-016: a cancelled turn leaves no span open and closes the agent span
/// the way the reference closes it. Reference `_conversation_loop` returns
/// normally on a user cancellation (`_loop.py:1668`), so `_safe_span` reads
/// no exception and sets `OK`; only a cancellation that unwinds the turn
/// leaves the status unset, because `_safe_span` decides a status for an
/// `Exception` and nothing else.
#[tokio::test]
async fn a_cancelled_turn_closes_its_agent_span() {
    let _exclusive = crate::tracing::harness::exclusive();
    let harness = crate::tracing::harness::Harness::install();
    let cancellation = CancellationToken::default();
    cancellation.cancel();
    let outcome = ConversationEngine::new(ScriptedProvider::new([Ok(completion(
        "unreached",
        Vec::new(),
    ))]))
    .run_turn("session-cancelled", provider_input(), "hello", cancellation)
    .await
    .expect("cancellation is an outcome");
    assert_eq!(outcome.stop_reason, TurnStopReason::Cancelled);
    let spans = harness.drain();
    drop(harness);
    let agent = spans
        .iter()
        .filter(|span| {
            span.name == "invoke_agent mistral-vibe"
                && span.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == "gen_ai.conversation.id"
                        && attribute.value.as_str() == "session-cancelled"
                })
        })
        .collect::<Vec<_>>();
    assert_eq!(agent.len(), 1, "the turn closed exactly one agent span");
    assert_eq!(agent[0].status, opentelemetry::trace::Status::Ok);
    let trace = agent[0].span_context.trace_id();
    let opened = spans
        .iter()
        .filter(|span| span.span_context.trace_id() == trace)
        .count();
    assert_eq!(
        opened, 1,
        "a cancelled turn opened no model call and no tool call"
    );
}

fn provider_input() -> ProviderInput {
    ProviderInput {
        dialect: Default::default(),
        turn_id: None,
        session_id: None,
        model_override: None,
        model: None,
        messages: vec![ModelMessage::System {
            content: "system".to_owned(),
        }],
        stream: true,
        images: Vec::new(),
        tools: Vec::new(),
        tool_choice: None,
        thinking: false,
        reasoning_effort: None,
        headers: BTreeMap::new(),
        limits: RequestLimits::default(),
        metadata: BTreeMap::new(),
    }
}

fn completion(text: &str, calls: Vec<ModelToolCall>) -> AssistantMessage {
    AssistantMessage {
        text: text.to_owned(),
        reasoning: None,
        reasoning_payloads: Vec::new(),
        tool_calls: calls,
        usage: Usage {
            input_tokens: 2,
            output_tokens: 3,
            cached_tokens: 0,
        },
        refusal: None,
        stop_reason: "stop".to_owned(),
        correlation_id: None,
    }
}

#[tokio::test]
async fn tool_cycles_checkpoint_and_finalize_once() {
    let provider = ScriptedProvider::new([
        Ok(completion(
            "",
            vec![ModelToolCall {
                id: "call-1".to_owned(),
                name: "read".to_owned(),
                arguments: "{}".to_owned(),
                presentation: None,
            }],
        )),
        Ok(completion("done", Vec::new())),
    ]);
    let outcome = ConversationEngine::new(provider)
        .with_tools(FakeTools)
        .run_turn(
            "session-1",
            provider_input(),
            "hello",
            CancellationToken::default(),
        )
        .await
        .expect("turn completes");
    assert_eq!(outcome.stop_reason, TurnStopReason::Complete);
    assert_eq!(outcome.steps, 3);
    assert_eq!(outcome.usage.output_tokens, 6);
    assert!(matches!(
        outcome.messages.last(),
        Some(ModelMessage::Assistant { content, .. }) if content == "done"
    ));
    assert_eq!(
        outcome
            .events
            .iter()
            .filter(|event| matches!(
                event.event,
                EngineEvent::Lifecycle {
                    state: LifecycleState::Completed,
                    ..
                }
            ))
            .count(),
        1
    );
}

/// US-009: every model call reports the request it is about to make, with
/// the model, the profile and the user message the events of that request
/// carry. Reference `send_request_sent`'s call site in the agent loop.
#[tokio::test]
async fn every_model_call_reports_the_request_it_makes() {
    let provider = ScriptedProvider::new([
        Ok(completion(
            "",
            vec![ModelToolCall {
                id: "call-1".to_owned(),
                name: "first".to_owned(),
                arguments: "{}".to_owned(),
                presentation: None,
            }],
        )),
        Ok(completion("done", Vec::new())),
    ]);
    let observer = Arc::new(RecordingObserver::default());
    let recorded = Arc::clone(&observer);
    let mut input = provider_input();
    input.turn_id = Some("turn-1".to_owned());
    input.model_override = Some("oracle-model".to_owned());
    ConversationEngine::new(provider)
        .with_tools(FakeTools)
        .with_agent_profile("oracle-profile")
        .with_observer(observer)
        .run_turn("session-1", input, "hello", CancellationToken::default())
        .await
        .expect("the turn runs");

    let events = recorded.events.lock().expect("observer lock").clone();
    let requests = events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::RequestSent {
                model,
                agent_profile,
                nb_context_messages,
                nb_prompt_chars,
                message_id,
                ..
            } => Some((
                model.clone(),
                agent_profile.clone(),
                *nb_context_messages,
                *nb_prompt_chars,
                message_id.clone(),
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        requests.len(),
        2,
        "one request is reported per model call, the tool answer included"
    );
    let (model, profile, messages, prompt_chars, message_id) = &requests[0];
    assert_eq!(model, "oracle-model");
    assert_eq!(profile, "oracle-profile");
    assert_eq!(*messages, 2, "the system prompt and the operator's turn");
    assert_eq!(*prompt_chars, 5);
    assert_eq!(
        message_id.as_deref(),
        Some("entry-turn-1-1"),
        "the identifier is the entry the operator's message was projected as"
    );
    assert!(
        requests[1].2 > *messages,
        "the second call carries the assistant turn and the tool answer too"
    );
}

#[tokio::test]
async fn parallel_tool_events_and_transcript_follow_arrival() {
    let provider = ScriptedProvider::new([
        Ok(completion(
            "",
            vec![
                ModelToolCall {
                    id: "call-1".to_owned(),
                    name: "first".to_owned(),
                    arguments: "{}".to_owned(),
                    presentation: None,
                },
                ModelToolCall {
                    id: "call-2".to_owned(),
                    name: "second".to_owned(),
                    arguments: "{}".to_owned(),
                    presentation: None,
                },
            ],
        )),
        Ok(completion("done", Vec::new())),
    ]);
    let tools = ControlledTools::default();
    let controls = tools.clone();
    let observer = Arc::new(RecordingObserver::default());
    let recorded = observer.clone();
    let task = tokio::spawn(async move {
        ConversationEngine::new(provider)
            .with_tools(tools)
            .with_observer(observer)
            .run_turn(
                "session-1",
                provider_input(),
                "hello",
                CancellationToken::default(),
            )
            .await
    });

    controls.all_started.notified().await;
    recorded.stream_seen.notified().await;
    assert!(
        recorded
            .events
            .lock()
            .expect("observer lock")
            .iter()
            .all(|event| !matches!(event, EngineEvent::ToolResult { .. }))
    );
    controls.release_second.notify_one();
    recorded.result_seen.notified().await;
    let first_completed_call = recorded
        .events
        .lock()
        .expect("observer lock")
        .iter()
        .find_map(|event| match event {
            EngineEvent::ToolResult { call_id, .. } => Some(call_id.clone()),
            _ => None,
        });
    assert_eq!(first_completed_call.as_deref(), Some("call-2"));
    controls.release_first.notify_one();

    let outcome = task.await.expect("engine joins").expect("turn completes");
    let tool_messages = outcome
        .messages
        .iter()
        .filter_map(|message| match message {
            ModelMessage::Tool {
                call_id, content, ..
            } => Some((call_id.as_str(), content.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    // Reference `_handle_tool_response` appends each answer as its call
    // settles, so the transcript keeps arrival order too.
    assert_eq!(
        tool_messages,
        vec![("call-2", "second-result"), ("call-1", "first-result")]
    );
    let events = recorded.events.lock().expect("observer lock");
    let second_stream = events.iter().position(|event| {
        matches!(
            event,
            EngineEvent::ToolStream { call_id, chunk }
                if call_id == "call-2" && chunk == "second-chunk"
        )
    });
    let second_result = events.iter().position(|event| {
        matches!(
            event,
            EngineEvent::ToolResult { call_id, .. } if call_id == "call-2"
        )
    });
    assert!(second_stream < second_result);
}

#[tokio::test]
async fn reactive_compaction_retries_without_spending_step_budget() {
    let provider = ScriptedProvider::new([
        Err(ProviderError::ContextOverflow),
        Ok(completion("after compact", Vec::new())),
    ]);
    let outcome = ConversationEngine::new(provider)
        .with_compactor(FakeCompactor)
        .run_turn(
            "session-1",
            provider_input(),
            "hello",
            CancellationToken::default(),
        )
        .await
        .expect("compacted turn completes");
    assert_eq!(outcome.session_id, "session-2");
    assert_eq!(outcome.steps, 2);
    assert!(
        outcome
            .events
            .iter()
            .any(|event| matches!(event.event, EngineEvent::SessionHandoff { .. }))
    );
}

/// US-155: the calls a compaction makes are credited to the turn, so the
/// token and price ceilings an operator set cover every request the tool
/// makes rather than only the ones the operator asked for. The compaction
/// still spends no step, and the context size it zeroed is recomputed by the
/// completion that follows it.
#[tokio::test]
async fn a_compaction_credits_its_own_calls_to_the_turn_ledger() {
    let provider = ScriptedProvider::new([
        Err(ProviderError::ContextOverflow),
        Ok(completion("after compact", Vec::new())),
    ]);
    let outcome = ConversationEngine::new(provider)
        .with_compactor(FakeCompactor)
        .with_limits(EngineLimits {
            input_price_per_million_micros: 1_000_000,
            output_price_per_million_micros: 2_000_000,
            ..EngineLimits::default()
        })
        .run_turn(
            "session-1",
            provider_input(),
            "hello",
            CancellationToken::default(),
        )
        .await
        .expect("compacted turn completes");

    // The one completion spends 2 and 3; the compaction spends 30 and 5.
    assert_eq!(
        outcome.usage,
        Usage {
            input_tokens: 32,
            output_tokens: 8,
            cached_tokens: 0,
        },
        "the summarization is part of what the turn spent"
    );
    assert_eq!(
        outcome.price_micros,
        32 + 16,
        "the price ceiling is evaluated against a total that includes the compaction"
    );
    assert_eq!(
        outcome.steps, 2,
        "the user message and the one completion are the only steps"
    );
    assert_eq!(
        outcome.context_tokens, 5,
        "the compaction zeroed the context size and the completion recomputed it"
    );
}

/// A tool that accepts a plan clears the context from inside the turn, the
/// way the reference does: the transcript is dropped, the session rotates,
/// and the continuation is the only thing the next request carries.
#[tokio::test]
async fn a_cleared_context_rotates_the_session_and_keeps_only_the_continuation() {
    struct ClearingTools {
        controls: TurnControlHandle,
    }

    impl ToolExecutor for ClearingTools {
        fn execute<'a>(&'a self, _name: &'a str, _arguments: &'a str) -> ToolFuture<'a> {
            Box::pin(async move {
                self.controls.send(TurnControl::ClearContext {
                    continuation: "Plan approved. Switch to code mode.".to_owned(),
                    plan_file_path: Some("/plans/session-1.md".to_owned()),
                });
                Ok(ToolExecutionOutput::text("plan accepted"))
            })
        }
    }

    let provider = ScriptedProvider::new([
        Ok(completion(
            "",
            vec![ModelToolCall {
                id: "call-1".to_owned(),
                name: "exit_plan_mode".to_owned(),
                arguments: "{}".to_owned(),
                presentation: None,
            }],
        )),
        Ok(completion("implementing", Vec::new())),
    ]);
    let controls = TurnControlHandle::default();
    let outcome = ConversationEngine::new(provider)
        .with_tools(ClearingTools {
            controls: controls.clone(),
        })
        .with_compactor(FakeCompactor)
        .run_turn_controlled(
            "session-1",
            provider_input(),
            "write a plan",
            CancellationToken::default(),
            controls,
        )
        .await
        .expect("cleared turn completes");

    assert_eq!(outcome.session_id, "session-1-cleared");
    // The reply carries the identity its public entry was minted under,
    // which is random, so it is compared apart.
    let mut messages = outcome.messages.clone();
    if let Some(ModelMessage::Assistant { message_id, .. }) = messages.last_mut() {
        assert!(
            message_id.take().is_some(),
            "the reply keeps its entry identity"
        );
    }
    // The continuation is persisted before the step closes, which mints
    // its identity as well.
    if let Some(ModelMessage::User { message_id, .. }) = messages.get_mut(1) {
        assert!(
            message_id.take().is_some(),
            "the continuation keeps its entry identity"
        );
    }
    assert_eq!(
        messages,
        vec![
            ModelMessage::System {
                content: "system".to_owned(),
            },
            ModelMessage::injected_user("Plan approved. Switch to code mode.".to_owned()),
            ModelMessage::Assistant {
                message_id: None,
                reasoning_message_id: None,
                content: "implementing".to_owned(),
                reasoning: None,
                reasoning_payloads: Vec::new(),
                tool_calls: Vec::new(),
                keeps_empty_content: false,
            },
        ],
        "clearing keeps the harness and the continuation, and nothing that was said"
    );
    let handoff = outcome
        .events
        .iter()
        .find_map(|event| match &event.event {
            EngineEvent::SessionHandoff {
                from_session_id,
                to_session_id,
                cause,
            } => Some((
                from_session_id.clone(),
                to_session_id.clone(),
                cause.clone(),
            )),
            _ => None,
        })
        .expect("the clearing publishes a handoff");
    assert_eq!(
        handoff,
        (
            "session-1".to_owned(),
            "session-1-cleared".to_owned(),
            SessionHandoffCause::ContextCleared {
                plan_file_path: Some("/plans/session-1.md".to_owned()),
            },
        )
    );
}

/// A compactor that cannot mint a cleared identifier fails the turn instead
/// of rotating onto its own name, which a reference projection rejects.
#[tokio::test]
async fn clearing_without_a_compactor_fails_the_turn() {
    struct ClearingTools {
        controls: TurnControlHandle,
    }

    impl ToolExecutor for ClearingTools {
        fn execute<'a>(&'a self, _name: &'a str, _arguments: &'a str) -> ToolFuture<'a> {
            Box::pin(async move {
                self.controls.send(TurnControl::ClearContext {
                    continuation: "continue".to_owned(),
                    plan_file_path: None,
                });
                Ok(ToolExecutionOutput::text("plan accepted"))
            })
        }
    }

    let provider = ScriptedProvider::new([
        Ok(completion(
            "",
            vec![ModelToolCall {
                id: "call-1".to_owned(),
                name: "exit_plan_mode".to_owned(),
                arguments: "{}".to_owned(),
                presentation: None,
            }],
        )),
        Ok(completion("unreachable", Vec::new())),
    ]);
    let controls = TurnControlHandle::default();
    let error = ConversationEngine::new(provider)
        .with_tools(ClearingTools {
            controls: controls.clone(),
        })
        .run_turn_controlled(
            "session-1",
            provider_input(),
            "write a plan",
            CancellationToken::default(),
            controls,
        )
        .await
        .expect_err("a clearing with no compactor cannot rotate the session");
    assert!(
        matches!(&error, EngineError::Compaction(message) if message.contains("clearing")),
        "the failure names what could not be done: {error}"
    );
}

#[tokio::test]
async fn limits_finish_with_typed_public_status() {
    // Limits are read before each request, so the turn needs a second
    // cycle for the first completion's usage to stop it.
    let provider = ScriptedProvider::new([Ok(completion(
        "too costly",
        vec![ModelToolCall {
            id: "call-1".to_owned(),
            name: "read".to_owned(),
            arguments: "{}".to_owned(),
            presentation: None,
        }],
    ))]);
    let outcome = ConversationEngine::new(provider)
        .with_tools(FakeTools)
        .with_limits(EngineLimits {
            max_total_tokens: 4,
            ..EngineLimits::default()
        })
        .run_turn(
            "session-1",
            provider_input(),
            "hello",
            CancellationToken::default(),
        )
        .await
        .expect("limit is an outcome");
    assert_eq!(outcome.stop_reason, TurnStopReason::TokenLimit);
    assert_eq!(outcome.snapshot.lifecycle, LifecycleState::Completed);
}

#[tokio::test]
async fn every_terminal_limit_has_a_typed_outcome() {
    let max_steps = ConversationEngine::new(ScriptedProvider::default())
        .with_limits(EngineLimits {
            max_steps: 0,
            ..EngineLimits::default()
        })
        .run_turn(
            "session-1",
            provider_input(),
            "hello",
            CancellationToken::default(),
        )
        .await
        .expect("max steps is an outcome");
    assert_eq!(max_steps.stop_reason, TurnStopReason::MaxSteps);

    let price = ConversationEngine::new(ScriptedProvider::new([Ok(completion(
        "cost",
        vec![ModelToolCall {
            id: "call-1".to_owned(),
            name: "read".to_owned(),
            arguments: "{}".to_owned(),
            presentation: None,
        }],
    ))]))
    .with_tools(FakeTools)
    .with_limits(EngineLimits {
        max_price_micros: 4,
        input_price_per_million_micros: 1_000_000,
        output_price_per_million_micros: 1_000_000,
        ..EngineLimits::default()
    })
    .run_turn(
        "session-1",
        provider_input(),
        "hello",
        CancellationToken::default(),
    )
    .await
    .expect("price limit is an outcome");
    assert_eq!(price.stop_reason, TurnStopReason::PriceLimit);
    assert_eq!(price.price_micros, 5);
    assert_eq!(price.snapshot.lifecycle, LifecycleState::Completed);
    assert!(price.messages.iter().any(
        |message| matches!(message, ModelMessage::Assistant { content, .. } if content == "cost")
    ));

    let refusal = ConversationEngine::new(ScriptedProvider::new([Err(ProviderError::Refusal(
        "policy".to_owned(),
    ))]))
    .run_turn(
        "session-1",
        provider_input(),
        "hello",
        CancellationToken::default(),
    )
    .await
    .expect("refusal is an outcome");
    assert_eq!(refusal.stop_reason, TurnStopReason::Refusal);

    let response_length =
        ConversationEngine::new(ScriptedProvider::new([Ok(completion("long", Vec::new()))]))
            .with_limits(EngineLimits {
                max_response_bytes: 3,
                ..EngineLimits::default()
            })
            .run_turn(
                "session-1",
                provider_input(),
                "hello",
                CancellationToken::default(),
            )
            .await
            .expect("response length is an outcome");
    assert_eq!(response_length.stop_reason, TurnStopReason::ResponseLength);
}

#[tokio::test]
async fn resumed_session_limits_include_the_persisted_usage_baseline() {
    let outcome = ConversationEngine::new(ScriptedProvider::default())
        .with_limits(EngineLimits {
            max_total_tokens: 4,
            input_price_per_million_micros: 1_000_000,
            output_price_per_million_micros: 1_000_000,
            ..EngineLimits::default()
        })
        .with_baseline(SessionStats {
            last_call: None,
            usage: Usage {
                input_tokens: 3,
                output_tokens: 2,
                cached_tokens: 0,
            },
            context_tokens: 5,
            steps: 1,
            tool_calls: ToolCallTally::default(),
        })
        .run_turn(
            "session-1",
            provider_input(),
            "hello",
            CancellationToken::default(),
        )
        .await
        .expect("baseline limit is a typed outcome");
    assert_eq!(outcome.stop_reason, TurnStopReason::TokenLimit);
    assert_eq!(outcome.usage.input_tokens, 3);
    assert_eq!(outcome.usage.output_tokens, 2);
    assert_eq!(outcome.context_tokens, 5);
    assert_eq!(outcome.steps, 2);
    assert_eq!(outcome.price_micros, 5);
    assert_eq!(outcome.snapshot.lifecycle, LifecycleState::Completed);
}

#[tokio::test]
async fn pre_cancelled_turn_finalizes_once_without_provider_work() {
    let cancellation = CancellationToken::default();
    cancellation.cancel();
    let outcome = ConversationEngine::new(ScriptedProvider::default())
        .run_turn("session-1", provider_input(), "hello", cancellation)
        .await
        .expect("cancellation is an outcome");
    assert_eq!(outcome.stop_reason, TurnStopReason::Cancelled);
    assert_eq!(outcome.steps, 1, "the user message is the only step");
    assert_eq!(outcome.snapshot.lifecycle, LifecycleState::Cancelled);
}

#[tokio::test]
async fn cancellation_during_persistence_drains_then_finalizes_once() {
    let sink = BlockingSink::default();
    let observer = sink.clone();
    let cancellation = CancellationToken::default();
    let cancel = cancellation.clone();
    let task = tokio::spawn(async move {
        ConversationEngine::new(ScriptedProvider::default())
            .with_sink(sink)
            .run_turn("session-1", provider_input(), "hello", cancellation)
            .await
    });
    observer.started.notified().await;
    cancel.cancel();
    observer.release.notify_one();
    let outcome = task
        .await
        .expect("engine task joins")
        .expect("cancellation is an outcome");
    assert_eq!(outcome.stop_reason, TurnStopReason::Cancelled);
    assert_eq!(outcome.snapshot.lifecycle, LifecycleState::Cancelled);
    assert_eq!(
        outcome
            .events
            .iter()
            .filter(|event| matches!(
                event.event,
                EngineEvent::Lifecycle {
                    state: LifecycleState::Cancelled,
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(observer.calls.load(AtomicOrdering::SeqCst), 2);
}

#[tokio::test]
async fn cancellation_repairs_every_declared_tool_result() {
    let provider = ScriptedProvider::new([Ok(completion(
        "",
        vec![
            ModelToolCall {
                id: "call-1".to_owned(),
                name: "first".to_owned(),
                arguments: "{}".to_owned(),
                presentation: None,
            },
            ModelToolCall {
                id: "call-2".to_owned(),
                name: "second".to_owned(),
                arguments: "{}".to_owned(),
                presentation: None,
            },
        ],
    ))]);
    let tools = BlockingTools::default();
    let observer = tools.clone();
    let cancellation = CancellationToken::default();
    let cancel = cancellation.clone();
    let task = tokio::spawn(async move {
        ConversationEngine::new(provider)
            .with_tools(tools)
            .run_turn("session-1", provider_input(), "hello", cancellation)
            .await
    });
    observer.started.notified().await;
    cancel.cancel();
    let outcome = task
        .await
        .expect("engine task joins")
        .expect("cancellation is an outcome");
    assert_eq!(outcome.stop_reason, TurnStopReason::Cancelled);
    assert_eq!(
        outcome
            .messages
            .iter()
            .filter(|message| matches!(message, ModelMessage::Tool { is_error: true, .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn cancellation_during_provider_stream_finalizes_once() {
    let provider = BlockingProvider::default();
    let observer = provider.clone();
    let cancellation = CancellationToken::default();
    let cancel = cancellation.clone();
    let task = tokio::spawn(async move {
        ConversationEngine::new(provider)
            .run_turn("session-1", provider_input(), "hello", cancellation)
            .await
    });
    observer.started.notified().await;
    cancel.cancel();
    let outcome = task
        .await
        .expect("engine task joins")
        .expect("cancellation is an outcome");
    assert_eq!(outcome.stop_reason, TurnStopReason::Cancelled);
    assert_eq!(outcome.snapshot.lifecycle, LifecycleState::Cancelled);
}

#[tokio::test]
async fn cancellation_during_compaction_finalizes_once() {
    let compactor = BlockingCompactor::default();
    let observer = compactor.clone();
    let cancellation = CancellationToken::default();
    let cancel = cancellation.clone();
    let task = tokio::spawn(async move {
        ConversationEngine::new(ScriptedProvider::new([Err(ProviderError::ContextOverflow)]))
            .with_compactor(compactor)
            .run_turn("session-1", provider_input(), "hello", cancellation)
            .await
    });
    observer.started.notified().await;
    cancel.cancel();
    let outcome = task
        .await
        .expect("engine task joins")
        .expect("cancellation is an outcome");
    assert_eq!(outcome.stop_reason, TurnStopReason::Cancelled);
    assert_eq!(outcome.snapshot.lifecycle, LifecycleState::Cancelled);
    // US-151: a cancelled compaction reports its status and publishes no
    // end event, so nothing renders a handoff that never happened.
    assert!(outcome.events.iter().any(|envelope| matches!(
        envelope.event,
        EngineEvent::CompactionOutcome {
            status: CompactionStatus::Cancelled,
            ..
        }
    )));
    assert!(!outcome.events.iter().any(|envelope| matches!(
        envelope.event,
        EngineEvent::CompactionCompleted { .. } | EngineEvent::SessionHandoff { .. }
    )));
}

#[tokio::test]
async fn transcript_checkpoints_append_and_rewrite_a_diverged_history() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = SessionStore::new(temporary.path()).with_pointer_key("sink");
    let metadata = store
        .create("session-sink", "/workspace", None, 10)
        .expect("session creates");
    let sink = SessionTranscriptSink::new(store.clone(), metadata);
    let snapshot = ProjectionReducer::new("session-sink").state().clone();
    let system = ModelMessage::System {
        content: "system".to_owned(),
    };
    let first = ModelMessage::user("first".to_owned());
    let second = ModelMessage::user("second".to_owned());

    sink.persist(&[system.clone(), first.clone()], &snapshot)
        .await
        .expect("first checkpoint");
    sink.persist(&[system.clone(), first.clone(), second.clone()], &snapshot)
        .await
        .expect("appending checkpoint");
    assert_eq!(
        store
            .load("session-sink")
            .expect("loads")
            .messages
            .iter()
            .filter_map(|message| match message {
                ModelMessage::User { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["first", "second"]
    );

    // A rewind shortens the history: the log must be rewritten, not extended.
    sink.persist(&[system, first], &snapshot)
        .await
        .expect("rewound checkpoint");
    let stored = store.load("session-sink").expect("loads");
    assert_eq!(stored.messages.len(), 1);
    assert!(matches!(
        stored.messages.first(),
        Some(ModelMessage::User { content, .. }) if content == "first"
    ));
}

/// US-158: the reference's `keep_parent` distinction, taken from the same
/// persistence path both rotations use. A compacted session continues the
/// one it summarized; a cleared session continues nothing, because what it
/// would point at was discarded.
#[tokio::test]
async fn a_compaction_keeps_the_parent_a_clearing_does_not() {
    struct ClearingTools {
        controls: TurnControlHandle,
    }

    impl ToolExecutor for ClearingTools {
        fn execute<'a>(&'a self, _name: &'a str, _arguments: &'a str) -> ToolFuture<'a> {
            Box::pin(async move {
                self.controls.send(TurnControl::ClearContext {
                    continuation: "start over".to_owned(),
                    plan_file_path: None,
                });
                Ok(ToolExecutionOutput::text("cleared"))
            })
        }
    }

    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = SessionStore::new(temporary.path()).with_pointer_key("parenting");

    // The threshold fires the compaction before the first request, so the
    // rotation the store sees is the compaction's.
    let compacted_metadata = store
        .create("session-compacted", "/workspace", None, 10)
        .expect("session creates");
    ConversationEngine::new(ScriptedProvider::new([Ok(completion("done", Vec::new()))]))
        .with_compactor(FakeCompactor)
        .with_sink(SessionTranscriptSink::new(
            store.clone(),
            compacted_metadata,
        ))
        .with_baseline(SessionStats {
            context_tokens: 1_000,
            ..SessionStats::default()
        })
        .with_compaction_settings(CompactionSettings {
            auto_compact_threshold: 100,
            ..CompactionSettings::default()
        })
        .run_turn(
            "session-compacted",
            provider_input(),
            "hello",
            CancellationToken::default(),
        )
        .await
        .expect("compacted turn completes");
    assert_eq!(
        store
            .load("session-2")
            .expect("the compacted session is durable")
            .metadata
            .parent_session_id
            .as_deref(),
        Some("session-compacted"),
    );

    let cleared_metadata = store
        .create("session-cleared", "/workspace", None, 20)
        .expect("session creates");
    let controls = TurnControlHandle::default();
    ConversationEngine::new(ScriptedProvider::new([
        Ok(completion(
            "",
            vec![ModelToolCall {
                id: "call-1".to_owned(),
                name: "exit_plan_mode".to_owned(),
                arguments: "{}".to_owned(),
                presentation: None,
            }],
        )),
        Ok(completion("restarted", Vec::new())),
    ]))
    .with_tools(ClearingTools {
        controls: controls.clone(),
    })
    .with_compactor(FakeCompactor)
    .with_sink(SessionTranscriptSink::new(store.clone(), cleared_metadata))
    .run_turn_controlled(
        "session-cleared",
        provider_input(),
        "write a plan",
        CancellationToken::default(),
        controls,
    )
    .await
    .expect("cleared turn completes");
    assert_eq!(
        store
            .load("session-cleared-cleared")
            .expect("the cleared session is durable")
            .metadata
            .parent_session_id,
        None,
        "a clearing retains no parent, matching `_reset_session(keep_parent=False)`"
    );
}

#[test]
fn provider_chunk_schema_remains_typed() {
    let chunk = ProviderChunk::Text {
        text: "hello".to_owned(),
    };
    assert_eq!(
        serde_json::to_value(chunk).expect("chunk serializes")["type"],
        "text"
    );
}

/// A policy whose answer is spent on the first poll, so a turn that would
/// otherwise ask for the same thing on every cycle terminates.
struct OnceThen {
    first: Mutex<Option<MiddlewareResult>>,
    resets: Mutex<Vec<ResetReason>>,
}

impl OnceThen {
    fn new(first: MiddlewareResult) -> Arc<Self> {
        Arc::new(Self {
            first: Mutex::new(Some(first)),
            resets: Mutex::new(Vec::new()),
        })
    }

    fn resets(&self) -> Vec<ResetReason> {
        self.resets
            .lock()
            .expect("reset log is not poisoned")
            .clone()
    }
}

impl ConversationMiddleware for OnceThen {
    fn before_turn(&self, _context: &ConversationContext<'_>) -> MiddlewareResult {
        self.first
            .lock()
            .expect("policy state is not poisoned")
            .take()
            .unwrap_or_default()
    }

    fn reset(&self, reset_reason: ResetReason) {
        self.resets
            .lock()
            .expect("reset log is not poisoned")
            .push(reset_reason);
    }
}

#[tokio::test]
async fn a_policy_stop_ends_the_turn_before_any_request_is_built() {
    // The scripted provider holds no response, so reaching it at all would
    // fail the turn rather than stop it.
    let outcome = ConversationEngine::new(ScriptedProvider::default())
        .with_middleware(OnceThen::new(MiddlewareResult::stop(
            "policy said so",
            TurnStopReason::PriceLimit,
        )))
        .run_turn(
            "session-1",
            provider_input(),
            "hello",
            CancellationToken::default(),
        )
        .await
        .expect("a policy stop is an outcome, not an error");

    assert_eq!(outcome.stop_reason, TurnStopReason::PriceLimit);
    assert_eq!(outcome.steps, 1, "the user message is the only step");
    assert!(
        outcome.events.iter().all(|envelope| match &envelope.event {
            EngineEvent::ModelText { text, .. } => {
                text == "<vibe_stop_event>policy said so</vibe_stop_event>"
            }
            _ => true,
        }),
        "no request was built, so the only model text is the stop event"
    );
}

#[tokio::test]
async fn cancellation_answers_before_any_policy_does() {
    let policy = OnceThen::new(MiddlewareResult::stop(
        "policy said so",
        TurnStopReason::PriceLimit,
    ));
    let cancellation = CancellationToken::default();
    cancellation.cancel();
    let outcome = ConversationEngine::new(ScriptedProvider::default())
        .with_middleware(Arc::clone(&policy) as Arc<dyn ConversationMiddleware>)
        .run_turn("session-1", provider_input(), "hello", cancellation)
        .await
        .expect("a cancelled turn is an outcome");

    assert_eq!(outcome.stop_reason, TurnStopReason::Cancelled);
    assert!(
        policy
            .first
            .lock()
            .expect("policy state is not poisoned")
            .is_some(),
        "the policy was never polled"
    );
}

#[tokio::test]
async fn an_injected_policy_message_rides_the_next_request() {
    let engine = ConversationEngine::new(ScriptedProvider::new([Ok(completion(
        "answered",
        Vec::new(),
    ))]))
    .with_middleware(OnceThen::new(MiddlewareResult::inject("half the window")));
    let outcome = engine
        .run_turn(
            "session-1",
            provider_input(),
            "hello",
            CancellationToken::default(),
        )
        .await
        .expect("the turn runs");

    assert_eq!(outcome.stop_reason, TurnStopReason::Complete);
    assert!(
        !outcome
            .events
            .iter()
            .any(|envelope| matches!(&envelope.event, EngineEvent::ContextInjected { .. })),
        "a policy's reminder publishes no history entry"
    );
    let requested = engine
        .provider
        .requests
        .lock()
        .expect("request log is not poisoned")
        .clone();
    assert_eq!(requested.len(), 1);
    assert!(
        requested[0].iter().any(|message| matches!(
            message,
            ModelMessage::User { content, .. } if content == "half the window"
        )),
        "the request that follows the injection carries it: {requested:?}"
    );
}

#[tokio::test]
async fn a_policy_compact_runs_before_any_request_and_resets_the_pipeline() {
    let policy = OnceThen::new(MiddlewareResult::compact());
    let engine = ConversationEngine::new(ScriptedProvider::new([Ok(completion(
        "answered",
        Vec::new(),
    ))]))
    .with_compactor(FakeCompactor)
    .with_middleware(Arc::clone(&policy) as Arc<dyn ConversationMiddleware>);
    let outcome = engine
        .run_turn(
            "session-1",
            provider_input(),
            "hello",
            CancellationToken::default(),
        )
        .await
        .expect("the turn runs");

    assert_eq!(outcome.stop_reason, TurnStopReason::Complete);
    let compaction_index = outcome
        .events
        .iter()
        .position(|envelope| matches!(envelope.event, EngineEvent::CompactionStarted { .. }))
        .expect("the compaction path was entered");
    let first_text = outcome
        .events
        .iter()
        .position(|envelope| matches!(envelope.event, EngineEvent::ModelText { .. }))
        .expect("a request was built afterward");
    assert!(
        compaction_index < first_text,
        "compaction happens before the request it makes room for"
    );
    let requested = engine
        .provider
        .requests
        .lock()
        .expect("request log is not poisoned")
        .clone();
    assert_eq!(
        requested,
        vec![vec![ModelMessage::System {
            content: "summary".to_owned(),
        }]],
        "the only request was built from the compacted transcript"
    );
    assert_eq!(
        policy.resets(),
        vec![ResetReason::Stop, ResetReason::Compact],
        "the turn opened with a stop reset and the compaction added its own"
    );
}

#[tokio::test]
async fn a_reactive_compaction_resets_the_pipeline_too() {
    // An overflow recovery replaces the transcript exactly like a policy
    // compaction does, so anything a policy latched against the old one is
    // just as stale.
    let policy = OnceThen::new(MiddlewareResult::default());
    let engine = ConversationEngine::new(ScriptedProvider::new([
        Err(ProviderError::ContextOverflow),
        Ok(completion("answered", Vec::new())),
    ]))
    .with_compactor(FakeCompactor)
    .with_middleware(Arc::clone(&policy) as Arc<dyn ConversationMiddleware>);
    let outcome = engine
        .run_turn(
            "session-1",
            provider_input(),
            "hello",
            CancellationToken::default(),
        )
        .await
        .expect("the turn recovers");

    assert_eq!(outcome.stop_reason, TurnStopReason::Complete);
    assert_eq!(
        policy.resets(),
        vec![ResetReason::Stop, ResetReason::Compact]
    );
}

#[tokio::test]
async fn every_user_turn_opens_with_a_stop_reset() {
    let policy = OnceThen::new(MiddlewareResult::default());
    let engine = ConversationEngine::new(ScriptedProvider::new([
        Ok(completion("first", Vec::new())),
        Ok(completion("second", Vec::new())),
    ]))
    .with_middleware(Arc::clone(&policy) as Arc<dyn ConversationMiddleware>);

    for prompt in ["hello", "again"] {
        engine
            .run_turn(
                "session-1",
                provider_input(),
                prompt,
                CancellationToken::default(),
            )
            .await
            .expect("the turn runs");
    }

    assert_eq!(policy.resets(), vec![ResetReason::Stop, ResetReason::Stop]);
}

/// US-149: the threshold fires before the request is built, from the
/// context size the ledger carries into the turn.
#[tokio::test]
async fn the_threshold_compacts_before_the_first_request() {
    let engine = ConversationEngine::new(ScriptedProvider::new([Ok(completion(
        "after compact",
        Vec::new(),
    ))]))
    .with_compactor(FakeCompactor)
    .with_baseline(SessionStats {
        context_tokens: 150_000,
        ..SessionStats::default()
    })
    .with_compaction_settings(CompactionSettings {
        auto_compact_threshold: 120_000,
        ..CompactionSettings::default()
    });
    let outcome = engine
        .run_turn(
            "session-1",
            provider_input(),
            "hello",
            CancellationToken::default(),
        )
        .await
        .expect("the turn runs");

    assert_eq!(outcome.stop_reason, TurnStopReason::Complete);
    assert_eq!(
        outcome.session_id, "session-2",
        "the threshold compaction rotated the session"
    );
    assert_eq!(
        engine
            .provider
            .requests
            .lock()
            .expect("request log is not poisoned")
            .clone(),
        vec![vec![ModelMessage::System {
            content: "summary".to_owned(),
        }]],
        "the only request was built from the compacted transcript, so no \
         provider overflow was ever reached"
    );
}

/// US-149: below the threshold nothing compacts, which is what makes the
/// test above a threshold test rather than a compactor test.
#[tokio::test]
async fn a_context_below_the_threshold_builds_its_request_untouched() {
    let outcome = ConversationEngine::new(ScriptedProvider::new([Ok(completion(
        "answered",
        Vec::new(),
    ))]))
    .with_compactor(FakeCompactor)
    .with_baseline(SessionStats {
        context_tokens: 119_999,
        ..SessionStats::default()
    })
    .with_compaction_settings(CompactionSettings {
        auto_compact_threshold: 120_000,
        ..CompactionSettings::default()
    })
    .run_turn(
        "session-1",
        provider_input(),
        "hello",
        CancellationToken::default(),
    )
    .await
    .expect("the turn runs");

    assert_eq!(outcome.session_id, "session-1");
    assert!(
        !outcome
            .events
            .iter()
            .any(|event| matches!(event.event, EngineEvent::CompactionStarted { .. }))
    );
}

/// US-149: the value the policy reads is the one `record_completion` wrote,
/// which is the reference's `context_tokens`: the last completion's prompt
/// and completion tokens, not the session totals.
#[tokio::test]
async fn the_threshold_reads_the_last_completion_usage() {
    let engine = ConversationEngine::new(ScriptedProvider::new([
        Ok(completion(
            "",
            vec![ModelToolCall {
                id: "call-1".to_owned(),
                name: "read".to_owned(),
                arguments: "{}".to_owned(),
                presentation: None,
            }],
        )),
        Ok(completion("done", Vec::new())),
    ]))
    .with_tools(FakeTools)
    .with_compactor(FakeCompactor)
    // Two completions of 5 tokens each spend 10 in total, and the context
    // is 5: a threshold of 6 must not fire.
    .with_compaction_settings(CompactionSettings {
        auto_compact_threshold: 6,
        ..CompactionSettings::default()
    });
    let outcome = engine
        .run_turn(
            "session-1",
            provider_input(),
            "hello",
            CancellationToken::default(),
        )
        .await
        .expect("the turn runs");

    assert_eq!(
        outcome.context_tokens, 5,
        "input plus output of the last completion"
    );
    assert_eq!(outcome.usage.input_tokens + outcome.usage.output_tokens, 10);
    assert!(
        !outcome
            .events
            .iter()
            .any(|event| matches!(event.event, EngineEvent::CompactionStarted { .. })),
        "the policy compares the context size, not the session total"
    );
}

/// US-150: an overflow is recovered from once per turn. A second one is
/// reported instead of compacting again.
#[tokio::test]
async fn a_second_overflow_in_one_turn_is_reported_rather_than_compacted() {
    let engine = ConversationEngine::new(ScriptedProvider::new([
        Err(ProviderError::ContextOverflow),
        Err(ProviderError::ContextOverflow),
        Ok(completion("never reached", Vec::new())),
    ]))
    .with_compactor(FakeCompactor);
    let error = engine
        .run_turn(
            "session-1",
            provider_input(),
            "hello",
            CancellationToken::default(),
        )
        .await
        .expect_err("the second overflow is not recovered from");
    assert!(
        matches!(error, EngineError::Provider(ProviderError::ContextOverflow)),
        "{error}"
    );
    assert_eq!(
        engine
            .provider
            .requests
            .lock()
            .expect("request log is not poisoned")
            .len(),
        2,
        "the turn stopped after the second overflow rather than retrying"
    );
}

/// US-150: the guard is per turn, so the next user turn may recover again.
#[tokio::test]
async fn a_new_turn_may_recover_from_an_overflow_again() {
    let engine = ConversationEngine::new(ScriptedProvider::new([
        Err(ProviderError::ContextOverflow),
        Ok(completion("first", Vec::new())),
        Err(ProviderError::ContextOverflow),
        Ok(completion("second", Vec::new())),
    ]))
    .with_compactor(FakeCompactor);
    for prompt in ["hello", "again"] {
        engine
            .run_turn(
                "session-1",
                provider_input(),
                prompt,
                CancellationToken::default(),
            )
            .await
            .unwrap_or_else(|error| panic!("the `{prompt}` turn recovers: {error}"));
    }
}

/// US-150: strict mode refuses the recovery outright, which is what
/// `_should_self_heal` reads `raise_on_compaction_failure` for.
#[tokio::test]
async fn strict_mode_reports_an_overflow_without_recovering() {
    let engine = ConversationEngine::new(ScriptedProvider::new([
        Err(ProviderError::ContextOverflow),
        Ok(completion("never reached", Vec::new())),
    ]))
    .with_compactor(FakeCompactor)
    .with_compaction_settings(CompactionSettings {
        raise_on_compaction_failure: true,
        ..CompactionSettings::default()
    });
    let error = engine
        .run_turn(
            "session-1",
            provider_input(),
            "hello",
            CancellationToken::default(),
        )
        .await
        .expect_err("strict mode surfaces the overflow");
    assert!(
        matches!(error, EngineError::Provider(ProviderError::ContextOverflow)),
        "{error}"
    );
    assert_eq!(
        engine
            .provider
            .requests
            .lock()
            .expect("request log is not poisoned")
            .len(),
        1,
        "no compaction and no retry was attempted"
    );
}

/// US-151: the boundary pair brackets the model call, and the outcome
/// reports the status the telemetry record carries.
#[tokio::test]
async fn a_compaction_publishes_its_boundary_pair_and_its_outcome() {
    let outcome = ConversationEngine::new(ScriptedProvider::new([Ok(completion(
        "after compact",
        Vec::new(),
    ))]))
    .with_compactor(FakeCompactor)
    .with_baseline(SessionStats {
        context_tokens: 150_000,
        ..SessionStats::default()
    })
    .with_compaction_settings(CompactionSettings {
        auto_compact_threshold: 120_000,
        ..CompactionSettings::default()
    })
    .run_turn(
        "session-1",
        provider_input(),
        "hello",
        CancellationToken::default(),
    )
    .await
    .expect("the turn runs");

    let compaction_events = outcome
        .events
        .iter()
        .filter(|envelope| {
            matches!(
                envelope.event,
                EngineEvent::CompactionStarted { .. }
                    | EngineEvent::CompactionCompleted { .. }
                    | EngineEvent::CompactionOutcome { .. }
            )
        })
        .map(|envelope| envelope.event.clone())
        .collect::<Vec<_>>();
    let [started, completed, reported] = compaction_events.as_slice() else {
        panic!("a compaction publishes three events, got {compaction_events:?}");
    };
    let EngineEvent::CompactionStarted {
        compaction_id,
        current_context_tokens,
        threshold,
    } = started
    else {
        unreachable!("the first event is the start");
    };
    assert_eq!(*current_context_tokens, 150_000);
    assert_eq!(*threshold, 120_000);
    assert_eq!(
        completed,
        &EngineEvent::CompactionCompleted {
            compaction_id: compaction_id.clone(),
            summary_length: 15,
            old_session_id: "session-1".to_owned(),
            new_session_id: "session-2".to_owned(),
        },
        "the pair is correlated and the length is the summary's characters"
    );
    assert_eq!(
        reported,
        &EngineEvent::CompactionOutcome {
            compaction_id: compaction_id.clone(),
            status: CompactionStatus::Success,
            context_tokens_before: 150_000,
            threshold: 120_000,
            reason: None,
        }
    );
    // The start is emitted before the model call the compaction makes, so a
    // client renders the operation rather than its aftermath.
    let started_at = outcome
        .events
        .iter()
        .position(|envelope| matches!(envelope.event, EngineEvent::CompactionStarted { .. }));
    let handoff_at = outcome
        .events
        .iter()
        .position(|envelope| matches!(envelope.event, EngineEvent::SessionHandoff { .. }));
    assert!(started_at < handoff_at);
}

/// US-151: a failed compaction reports its classified reason and publishes
/// no completed event, so nothing renders a handoff that never happened.
#[tokio::test]
async fn a_failed_compaction_reports_its_reason_and_no_end_event() {
    struct FailingCompactor;

    impl Compactor for FailingCompactor {
        fn compact<'a>(
            &'a self,
            _current_session_id: &'a str,
            _messages: &'a [ModelMessage],
        ) -> CompactionFuture<'a> {
            Box::pin(async {
                Err(CompactionFailure::classified(
                    CompactionFailureReason::EmptySummary,
                    "the summarizer answered with nothing",
                ))
            })
        }

        fn cleared_session_id(&self, _current_session_id: &str) -> Result<String, String> {
            Err("unused".to_owned())
        }
    }

    let recorded = Arc::new(RecordingObserver::default());
    let error =
        ConversationEngine::new(ScriptedProvider::new([Err(ProviderError::ContextOverflow)]))
            .with_compactor(FailingCompactor)
            .with_observer(Arc::clone(&recorded) as Arc<dyn EventObserver>)
            .run_turn(
                "session-1",
                provider_input(),
                "hello",
                CancellationToken::default(),
            )
            .await
            .expect_err("a failed compaction fails the turn");
    assert!(matches!(error, EngineError::Compaction(_)), "{error}");

    let events = recorded.events.lock().expect("observer lock").clone();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::CompactionStarted { .. })),
        "the start was published before the failure"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EngineEvent::CompactionCompleted { .. })),
        "a failure publishes no end event"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EngineEvent::SessionHandoff { .. })),
        "a failure records no handoff"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            EngineEvent::CompactionOutcome {
                status: CompactionStatus::Failure,
                reason: Some(CompactionFailureReason::EmptySummary),
                ..
            }
        )),
        "the outcome carries the classified reason: {events:?}"
    );
}

#[test]
fn a_transcript_written_before_the_pipeline_still_deserializes() {
    // The pipeline added no event variant and no message field, so every
    // envelope a previous release wrote still reads back.
    for stored in [
        json!({"sessionId": "s", "eventId": 1, "type": "user_message", "content": "hello"}),
        json!({"sessionId": "s", "eventId": 2, "type": "context_injected", "content": "note", "as_message": false}),
        json!({"sessionId": "s", "eventId": 3, "type": "compaction", "summary": "short"}),
    ] {
        serde_json::from_value::<EventEnvelope>(stored.clone())
            .unwrap_or_else(|error| panic!("{stored} still deserializes: {error}"));
    }
    let message: ModelMessage = serde_json::from_value(json!({"role": "user", "content": "hello"}))
        .expect("a stored user message still deserializes");
    assert_eq!(message, ModelMessage::user("hello".to_owned()));
}

/// The invoked-skill resolver the injection tests drive: `/probe` is the
/// one user-invocable skill, resolved the way the builtin resolver does.
struct StubSkills;

impl crate::skills::InvokedSkillResolver for StubSkills {
    fn resolve(&self, prompt: &str) -> Option<crate::skills::InvokedSkill> {
        let name = prompt
            .trim()
            .strip_prefix('/')?
            .split_whitespace()
            .next()?
            .to_ascii_lowercase();
        if name != "probe" {
            return None;
        }
        let marker = crate::skills::skill_content_marker("probe");
        Some(crate::skills::InvokedSkill {
            name: "probe".to_owned(),
            loaded: ToolExecutionOutput::new(format!(
                "name: probe\ncontent: {marker}\nDo the probing.\n</skill_content>\nskill_dir: None"
            ))
            .displayed_as(json!({"kind": "skill", "name": "probe"}))
            .typed(json!({"name": "probe"})),
            already_loaded: ToolExecutionOutput::new(
                "name: probe\ncontent: already loaded; reuse those instructions\nskill_dir: None"
                    .to_owned(),
            )
            .displayed_as(json!({"kind": "skill", "name": "probe"}))
            .typed(json!({"name": "probe"})),
        })
    }
}

/// US-172: a `/name` prompt appends the synthetic pair the reference
/// writes: an assistant message whose only content is one `skill` tool
/// call, then the tool message carrying the rendering, both before the
/// first model request, which therefore reads them.
#[tokio::test]
async fn a_slash_invocation_appends_the_synthetic_skill_pair() {
    let provider = ScriptedProvider::new([Ok(completion("done", Vec::new()))]);
    let observer = Arc::new(RecordingObserver::default());
    let outcome = ConversationEngine::new(provider)
        .with_observer(observer.clone())
        .with_invoked_skills(Arc::new(StubSkills))
        .run_turn(
            "session-1",
            provider_input(),
            "/probe extra instructions here",
            CancellationToken::default(),
        )
        .await
        .expect("turn completes");

    let messages = &outcome.messages;
    assert_eq!(
        messages[1],
        ModelMessage::User {
            content: "/probe extra instructions here".to_owned(),
            injected: false,
            message_id: Some("entry-1".to_owned()),
            attachments: Vec::new(),
            manual_shell: None,
            compaction_boundary: false,
            input_text: None,
            user_display_content: None,
        },
        "the trailing text stays the operator's message, under its entry's identity"
    );
    let ModelMessage::Assistant {
        content,
        tool_calls,
        ..
    } = &messages[2]
    else {
        panic!("the pair opens with an assistant message: {messages:?}");
    };
    assert_eq!(content, "", "the assistant message carries no text");
    assert_eq!(tool_calls.len(), 1, "exactly one call: {tool_calls:?}");
    assert_eq!(tool_calls[0].name, "skill");
    assert_eq!(
        serde_json::from_str::<Value>(&tool_calls[0].arguments).expect("arguments are JSON"),
        json!({"name": "probe"}),
    );
    let id = tool_calls[0].id.as_str();
    let shape = id.split('-').map(str::len).collect::<Vec<_>>();
    assert_eq!(shape, [8, 4, 4, 4, 12], "the locally minted id shape: {id}");
    assert!(
        id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
        "{id}"
    );
    let ModelMessage::Tool {
        call_id,
        content,
        is_error,
        ..
    } = &messages[3]
    else {
        panic!("the call is answered by a tool message: {messages:?}");
    };
    assert_eq!(call_id, id, "the result answers the minted call");
    assert!(!is_error);
    assert!(
        content.contains(&crate::skills::skill_content_marker("probe")),
        "the first load renders the body: {content}"
    );
    assert!(
        content.starts_with("name: probe\ncontent:"),
        "the result is the fields one per line: {content}"
    );

    let events = observer.events.lock().expect("events");
    let call_index = events
        .iter()
        .position(|event| matches!(event, EngineEvent::ToolCall { name, .. } if name == "skill"))
        .expect("the synthetic call is a turn event");
    assert!(
        matches!(
            &events[call_index + 1],
            EngineEvent::ToolResult {
                is_error: false,
                ..
            }
        ),
        "the result event follows the call: {events:?}"
    );
}

/// US-172: the second invocation of a skill already in the stored history
/// is acknowledged rather than rendered again, decided by searching the
/// stored tool messages of `skill` calls for the content marker.
#[tokio::test]
async fn a_second_invocation_is_acknowledged_from_the_stored_history() {
    let marker = crate::skills::skill_content_marker("probe");
    let mut input = provider_input();
    input.messages.extend([
        ModelMessage::Assistant {
            message_id: None,
            reasoning_message_id: None,
            content: String::new(),
            reasoning: None,
            reasoning_payloads: Vec::new(),
            tool_calls: vec![ModelToolCall {
                id: "earlier-call".to_owned(),
                name: "skill".to_owned(),
                arguments: "{\"name\": \"probe\"}".to_owned(),
                presentation: None,
            }],
            keeps_empty_content: false,
        },
        ModelMessage::Tool {
            call_id: "earlier-call".to_owned(),
            content: format!("name: probe\ncontent: {marker}\nDo the probing."),
            is_error: false,
            name: String::new(),
            result: None,
        },
    ]);
    let provider = ScriptedProvider::new([Ok(completion("done", Vec::new()))]);
    let outcome = ConversationEngine::new(provider)
        .with_invoked_skills(Arc::new(StubSkills))
        .run_turn("session-1", input, "/probe", CancellationToken::default())
        .await
        .expect("turn completes");

    let repeat = outcome
        .messages
        .iter()
        .filter(|message| matches!(message, ModelMessage::Tool { .. }))
        .nth(1)
        .expect("the invocation still appends a tool message");
    let ModelMessage::Tool { content, .. } = repeat else {
        unreachable!()
    };
    assert!(
        content.contains("already loaded"),
        "the repeat is acknowledged: {content}"
    );
    assert!(
        !content.contains(&marker),
        "the body is not rendered again: {content}"
    );
}

/// US-172: the marker alone does not mark a skill loaded; it has to sit in
/// the answer of a `skill` call, so a file read that happens to contain the
/// marker text does not swallow a later invocation.
#[tokio::test]
async fn a_marker_in_an_unrelated_tool_answer_does_not_count_as_loaded() {
    let mut input = provider_input();
    input.messages.extend([
        ModelMessage::Assistant {
            message_id: None,
            reasoning_message_id: None,
            content: String::new(),
            reasoning: None,
            reasoning_payloads: Vec::new(),
            tool_calls: vec![ModelToolCall {
                id: "read-call".to_owned(),
                name: "read".to_owned(),
                arguments: "{}".to_owned(),
                presentation: None,
            }],
            keeps_empty_content: false,
        },
        ModelMessage::Tool {
            call_id: "read-call".to_owned(),
            content: crate::skills::skill_content_marker("probe"),
            is_error: false,
            name: String::new(),
            result: None,
        },
    ]);
    let provider = ScriptedProvider::new([Ok(completion("done", Vec::new()))]);
    let outcome = ConversationEngine::new(provider)
        .with_invoked_skills(Arc::new(StubSkills))
        .run_turn("session-1", input, "/probe", CancellationToken::default())
        .await
        .expect("turn completes");

    let appended = outcome
        .messages
        .iter()
        .filter(|message| matches!(message, ModelMessage::Tool { .. }))
        .nth(1)
        .expect("the pair is appended");
    let ModelMessage::Tool { content, .. } = appended else {
        unreachable!()
    };
    assert!(
        !content.contains("already loaded"),
        "the unrelated marker did not count: {content}"
    );
}

/// US-172: a slash word naming no skill, or a session publishing no
/// resolver, leaves the prompt an ordinary message with no pair.
#[tokio::test]
async fn a_slash_word_naming_no_skill_is_an_ordinary_prompt() {
    let provider = ScriptedProvider::new([Ok(completion("done", Vec::new()))]);
    let observer = Arc::new(RecordingObserver::default());
    let outcome = ConversationEngine::new(provider)
        .with_observer(observer.clone())
        .with_invoked_skills(Arc::new(StubSkills))
        .run_turn(
            "session-1",
            provider_input(),
            "/unknown do things",
            CancellationToken::default(),
        )
        .await
        .expect("turn completes");

    assert!(
        !outcome
            .messages
            .iter()
            .any(|message| matches!(message, ModelMessage::Tool { .. })),
        "no pair was appended: {:?}",
        outcome.messages
    );
    assert!(
        !observer
            .events
            .lock()
            .expect("events")
            .iter()
            .any(|event| matches!(event, EngineEvent::ToolCall { .. })),
        "no synthetic call was emitted"
    );
}

/// US-173: the steer control carries the wire's `injectInvokedSkill`, so a
/// steer naming a skill appends the pair when the flag is true and stays a
/// plain message when it is false.
#[tokio::test]
async fn a_steer_injects_the_skill_only_when_its_flag_says_so() {
    for (inject, expected_pairs) in [(true, 1_usize), (false, 0_usize)] {
        let provider = ScriptedProvider::new([Ok(completion("done", Vec::new()))]);
        let controls = TurnControlHandle::default();
        controls.send(TurnControl::Steer {
            content: "/probe".to_owned(),
            inject_invoked_skill: inject,
        });
        let outcome = ConversationEngine::new(provider)
            .with_invoked_skills(Arc::new(StubSkills))
            .run_turn_controlled(
                "session-1",
                provider_input(),
                "first ask",
                CancellationToken::default(),
                controls,
            )
            .await
            .expect("turn completes");

        let pairs = outcome
            .messages
            .iter()
            .filter(|message| matches!(message, ModelMessage::Tool { .. }))
            .count();
        assert_eq!(
            pairs, expected_pairs,
            "injectInvokedSkill={inject}: {:?}",
            outcome.messages
        );
    }
}

/// US-172: the pair round-trips through serialization unchanged, so a
/// persisted transcript carrying one reloads as the conversation it was.
#[test]
fn the_synthetic_pair_round_trips_through_serialization() {
    use crate::skills::InvokedSkillResolver as _;
    let mut messages = vec![ModelMessage::user("/probe".to_owned())];
    let invoked = StubSkills.resolve("/probe").expect("resolves");
    crate::skills::append_invoked_skill(&mut messages, &invoked);
    let stored = serde_json::to_value(&messages).expect("serializes");
    let reloaded: Vec<ModelMessage> = serde_json::from_value(stored).expect("deserializes");
    assert_eq!(reloaded, messages);
}

//! What a turn needs from the outside, and the stand-in for each.
//!
//! The engine is generic over four collaborators: something that completes,
//! something that runs tools, something that compacts, and something that
//! persists. Each is a trait here, and each has a value that answers "nothing
//! is wired for this" without the caller having to build a second engine: the
//! turn behaves the same whether a collaborator is real or absent, which is
//! what makes a turn testable one collaborator at a time.
//!
//! [`CompletionProvider::stream`] is the one contract with a substantial
//! default. A provider that only knows how to complete gets streaming for free,
//! synthesized from the finished message in the chunk order a real stream would
//! have produced, so nothing downstream has to ask which kind it is holding.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use super::{
    CompactionFuture, PersistenceFuture, ProviderFuture, ProviderStreamFuture, SessionStats,
    ToolFuture, ToolStreamSink, current_time_millis,
};
use crate::compaction::CompactionFailure;
use crate::events::{
    EventEnvelope, ModelMessage, ProjectionSnapshot, RemoteToolOrigin, SessionHandoffCause,
};
use crate::llm::retry::{RetryObserver, RetryReason};
use crate::provider::{
    AssistantMessage, ModelCallDescriptor, ProviderChunk, ProviderInput, ProviderStream,
};
use crate::storage::{SessionMetadata, SessionStore};

pub trait CompletionProvider: Send + Sync {
    fn complete<'a>(&'a self, input: &'a ProviderInput) -> ProviderFuture<'a>;

    /// The model this provider addresses when a turn names no override.
    ///
    /// Reference `config.get_active_model().alias`, which the telemetry client
    /// reads back for every request and tool event. A provider that answers
    /// nothing leaves those events reporting the turn's own override.
    fn model(&self) -> Option<&str> {
        None
    }

    /// What the model call span reports about the request this provider makes.
    ///
    /// Reference opens that span inside the backend, which is the only layer
    /// that knows the provider, the API style and the URL. Here the span is
    /// opened by the turn, so the backend publishes those three and a provider
    /// that answers nothing leaves the span carrying what the turn knows.
    fn call_descriptor(&self) -> Option<ModelCallDescriptor> {
        None
    }

    /// Whether a call to this provider is reported under a model call span.
    /// Reference `GenericBackend` opens one and `MistralBackend` none.
    fn traces_model_calls(&self) -> bool {
        true
    }

    /// Streams one completion, reporting every retry to `retries`.
    ///
    /// A provider that does not retry never calls the sink, which is why the
    /// default forwards to [`CompletionProvider::stream`] instead of requiring
    /// every implementation to know about retries.
    fn stream_observed<'a>(
        &'a self,
        input: &'a ProviderInput,
        _retries: &'a (dyn RetryObserver + 'a),
    ) -> ProviderStreamFuture<'a> {
        self.stream(input)
    }

    fn stream<'a>(&'a self, input: &'a ProviderInput) -> ProviderStreamFuture<'a> {
        Box::pin(async move { Ok(chunks_of(self.complete(input).await?)) })
    }
}

/// A finished message as the stream a real provider would have produced, in
/// the same chunk order.
#[must_use]
pub fn chunks_of(message: AssistantMessage) -> ProviderStream<'static> {
    let mut chunks = Vec::new();
    if let Some(reasoning) = message.reasoning {
        chunks.push(ProviderChunk::Reasoning { text: reasoning });
    }
    chunks.extend(
        message
            .reasoning_payloads
            .into_iter()
            .map(|payload| ProviderChunk::ReasoningPayload { payload }),
    );
    chunks.push(ProviderChunk::Text { text: message.text });
    chunks.extend(
        message
            .tool_calls
            .into_iter()
            .map(|call| ProviderChunk::ToolCall {
                id: call.id,
                name: call.name,
                arguments: call.arguments,
            }),
    );
    chunks.push(ProviderChunk::Usage {
        input_tokens: message.usage.input_tokens,
        output_tokens: message.usage.output_tokens,
    });
    if let Some(message) = message.refusal {
        chunks.push(ProviderChunk::Refusal { message });
    }
    chunks.push(ProviderChunk::Stop {
        reason: message.stop_reason,
    });
    ProviderStream {
        correlation_id: message.correlation_id,
        chunks: Box::pin(futures_util::stream::iter(chunks.into_iter().map(Ok))),
    }
}

impl<P> CompletionProvider for Arc<P>
where
    P: CompletionProvider + ?Sized,
{
    fn complete<'a>(&'a self, input: &'a ProviderInput) -> ProviderFuture<'a> {
        (**self).complete(input)
    }

    fn stream<'a>(&'a self, input: &'a ProviderInput) -> ProviderStreamFuture<'a> {
        (**self).stream(input)
    }

    fn stream_observed<'a>(
        &'a self,
        input: &'a ProviderInput,
        retries: &'a (dyn RetryObserver + 'a),
    ) -> ProviderStreamFuture<'a> {
        (**self).stream_observed(input, retries)
    }

    fn model(&self) -> Option<&str> {
        (**self).model()
    }

    fn call_descriptor(&self) -> Option<ModelCallDescriptor> {
        (**self).call_descriptor()
    }

    fn traces_model_calls(&self) -> bool {
        (**self).traces_model_calls()
    }
}

/// Forwards a retry to the turn that is waiting on the request.
pub(super) struct ChannelRetrySink {
    pub(super) reasons: mpsc::UnboundedSender<RetryReason>,
}

impl RetryObserver for ChannelRetrySink {
    fn retrying(&self, reason: &RetryReason) {
        // A closed receiver means the turn is gone; the request still finishes.
        let _ = self.reasons.send(reason.clone());
    }
}

pub trait ToolExecutor: Send + Sync {
    /// The remote a published tool proxies, when the executor publishes one.
    ///
    /// The projection presents a proxied call from the name its server gave it,
    /// which only the registration holds, so the engine asks here before it
    /// raises the call event. An executor that publishes nothing remote keeps
    /// the default.
    fn remote_origin(&self, _name: &str) -> Option<RemoteToolOrigin> {
        None
    }

    /// Whether a call to `name` reaches a tool this executor publishes. The
    /// reference announces a streamed call only once it resolves the name to
    /// an available tool, so an unknown name is never announced.
    fn publishes(&self, _name: &str) -> bool {
        true
    }

    fn execute<'a>(&'a self, name: &'a str, arguments: &'a str) -> ToolFuture<'a>;

    fn execute_stream<'a>(
        &'a self,
        name: &'a str,
        arguments: &'a str,
        _output: ToolStreamSink,
    ) -> ToolFuture<'a> {
        self.execute(name, arguments)
    }

    /// Runs the call the model identified as `call_id`, which a tool that
    /// delegates or asks for approval names to the client.
    fn execute_call<'a>(
        &'a self,
        _call_id: &'a str,
        name: &'a str,
        arguments: &'a str,
        output: ToolStreamSink,
    ) -> ToolFuture<'a> {
        self.execute_stream(name, arguments, output)
    }
}

pub trait Compactor: Send + Sync {
    fn compact<'a>(
        &'a self,
        current_session_id: &'a str,
        messages: &'a [ModelMessage],
    ) -> CompactionFuture<'a>;

    /// Mints the identifier a cleared transcript continues under.
    ///
    /// Clearing borrows the compactor's naming authority without its summary:
    /// the transcript is dropped rather than condensed, but the session still
    /// rotates onto an identifier no other handoff has claimed.
    fn cleared_session_id(&self, current_session_id: &str) -> Result<String, String>;
}

pub trait TranscriptSink: Send + Sync {
    fn persist<'a>(
        &'a self,
        messages: &'a [ModelMessage],
        snapshot: &'a ProjectionSnapshot,
    ) -> PersistenceFuture<'a>;

    fn persist_stats<'a>(&'a self, _stats: &'a SessionStats) -> PersistenceFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}

pub trait EventObserver: Send + Sync {
    fn observe(&self, event: &EventEnvelope) -> Result<(), String>;
}

pub struct CompositeEventObserver {
    primary: Arc<dyn EventObserver>,
    secondary: Arc<dyn EventObserver>,
}

impl CompositeEventObserver {
    #[must_use]
    pub fn new(primary: Arc<dyn EventObserver>, secondary: Arc<dyn EventObserver>) -> Self {
        Self { primary, secondary }
    }
}

impl EventObserver for CompositeEventObserver {
    fn observe(&self, event: &EventEnvelope) -> Result<(), String> {
        self.primary.observe(event)?;
        self.secondary.observe(event)
    }
}

#[derive(Debug, Clone, Default)]
pub struct NoopEventObserver;

impl EventObserver for NoopEventObserver {
    fn observe(&self, _event: &EventEnvelope) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct NoTools;

impl ToolExecutor for NoTools {
    fn execute<'a>(&'a self, name: &'a str, _arguments: &'a str) -> ToolFuture<'a> {
        Box::pin(async move { Err(format!("tool `{name}` is unavailable")) })
    }
}

#[derive(Debug, Clone, Default)]
pub struct RejectCompaction;

impl Compactor for RejectCompaction {
    fn compact<'a>(
        &'a self,
        _current_session_id: &'a str,
        _messages: &'a [ModelMessage],
    ) -> CompactionFuture<'a> {
        Box::pin(async { Err(CompactionFailure::from("compaction is unavailable")) })
    }

    fn cleared_session_id(&self, _current_session_id: &str) -> Result<String, String> {
        Err("context clearing is unavailable".to_owned())
    }
}

#[derive(Debug, Clone, Default)]
pub struct NoopTranscriptSink;

impl TranscriptSink for NoopTranscriptSink {
    fn persist<'a>(
        &'a self,
        _messages: &'a [ModelMessage],
        _snapshot: &'a ProjectionSnapshot,
    ) -> PersistenceFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}

/// A sink a caller may or may not have, persisting through the one it has.
///
/// [`ConversationEngine::with_sink`] moves the sink into the engine's type, so a
/// caller that persists only sometimes would otherwise have to build the whole
/// engine twice, once per branch, and keep the two chains in step by hand. This
/// makes "no transcript" a value rather than a second type.
impl<S: TranscriptSink> TranscriptSink for Option<S> {
    fn persist<'a>(
        &'a self,
        messages: &'a [ModelMessage],
        snapshot: &'a ProjectionSnapshot,
    ) -> PersistenceFuture<'a> {
        match self {
            Some(sink) => sink.persist(messages, snapshot),
            None => Box::pin(async { Ok(()) }),
        }
    }

    fn persist_stats<'a>(&'a self, stats: &'a SessionStats) -> PersistenceFuture<'a> {
        match self {
            Some(sink) => sink.persist_stats(stats),
            None => Box::pin(async { Ok(()) }),
        }
    }
}

pub struct SessionTranscriptSink {
    store: SessionStore,
    metadata: Mutex<SessionMetadata>,
}

impl SessionTranscriptSink {
    #[must_use]
    pub fn new(store: SessionStore, metadata: SessionMetadata) -> Self {
        Self {
            store,
            metadata: Mutex::new(metadata),
        }
    }
}

impl TranscriptSink for SessionTranscriptSink {
    fn persist<'a>(
        &'a self,
        messages: &'a [ModelMessage],
        snapshot: &'a ProjectionSnapshot,
    ) -> PersistenceFuture<'a> {
        Box::pin(async move {
            let persisted_at = current_time_millis();
            let mut metadata = self
                .metadata
                .lock()
                .map_err(|_| "session metadata lock poisoned".to_owned())?;
            if snapshot.session_id != metadata.id {
                let handoff = self
                    .store
                    // The reference keeps the parent for a compaction and drops
                    // it for a clearing, which is the only thing its
                    // `keep_parent` flag decides.
                    .handoff_messages(
                        &metadata,
                        &snapshot.session_id,
                        messages,
                        persisted_at,
                        !matches!(
                            snapshot.handoff_cause,
                            Some(SessionHandoffCause::ContextCleared { .. })
                        ),
                    )
                    .map_err(|error| error.to_string())?;
                *metadata = handoff;
                return Ok(());
            }
            // The metadata already records how much of the log is persisted, so
            // a checkpoint never has to re-read the transcript it just wrote.
            let non_system: Vec<&ModelMessage> = messages
                .iter()
                .filter(|message| !matches!(message, ModelMessage::System { .. }))
                .collect();
            if !SessionStore::extends_persisted_log(&metadata, &non_system)
                .map_err(|error| error.to_string())?
            {
                return self
                    .store
                    .replace_messages(&mut metadata, messages, persisted_at)
                    .map_err(|error| error.to_string());
            }
            let persisted = usize::try_from(metadata.message_count).unwrap_or(usize::MAX);
            let pending: Vec<ModelMessage> = messages
                .iter()
                .find(|message| matches!(message, ModelMessage::System { .. }))
                .into_iter()
                .chain(non_system.into_iter().skip(persisted))
                .cloned()
                .collect();
            self.store
                .append_messages(&mut metadata, &pending, persisted_at)
                .map_err(|error| error.to_string())
        })
    }

    fn persist_stats<'a>(&'a self, stats: &'a SessionStats) -> PersistenceFuture<'a> {
        Box::pin(async move {
            let mut metadata = self
                .metadata
                .lock()
                .map_err(|_| "session metadata lock poisoned".to_owned())?;
            metadata.statistics.insert(
                "session_prompt_tokens".to_owned(),
                serde_json::Value::from(stats.usage.input_tokens),
            );
            metadata.statistics.insert(
                "session_completion_tokens".to_owned(),
                serde_json::Value::from(stats.usage.output_tokens),
            );
            metadata.statistics.insert(
                "context_tokens".to_owned(),
                serde_json::Value::from(stats.context_tokens),
            );
            metadata
                .statistics
                .insert("steps".to_owned(), serde_json::Value::from(stats.steps));
            // Reference `AgentStats.last_turn_*` and `tokens_per_second`: the
            // last model call, which a reopened session reports until its
            // next call replaces it.
            if let Some(call) = &stats.last_call {
                #[allow(clippy::cast_precision_loss)]
                let seconds = call.duration_ms as f64 / 1_000.0;
                #[allow(clippy::cast_precision_loss)]
                let tokens_per_second = call.completion_tokens as f64 / seconds;
                for (key, value) in [
                    (
                        "last_turn_prompt_tokens",
                        serde_json::json!(call.prompt_tokens),
                    ),
                    (
                        "last_turn_completion_tokens",
                        serde_json::json!(call.completion_tokens),
                    ),
                    ("last_turn_duration", serde_json::json!(seconds)),
                    ("tokens_per_second", serde_json::json!(tokens_per_second)),
                ] {
                    metadata.statistics.insert(key.to_owned(), value);
                }
            }
            self.store
                .update_metadata(&metadata)
                .map_err(|error| error.to_string())
        })
    }
}

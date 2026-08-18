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
use crate::events::{EventEnvelope, ModelMessage, ProjectionSnapshot, SessionHandoffCause};
use crate::provider::{
    ModelCallDescriptor, ProviderBackend, ProviderChunk, ProviderInput, ProviderStream,
    ProviderTransport, RetrySink,
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

    /// Streams one completion, reporting every retry to `retries`.
    ///
    /// A provider that does not retry never calls the sink, which is why the
    /// default forwards to [`CompletionProvider::stream`] instead of requiring
    /// every implementation to know about retries.
    fn stream_observed<'a>(
        &'a self,
        input: &'a ProviderInput,
        _retries: &'a (dyn RetrySink + 'a),
    ) -> ProviderStreamFuture<'a> {
        self.stream(input)
    }

    fn stream<'a>(&'a self, input: &'a ProviderInput) -> ProviderStreamFuture<'a> {
        Box::pin(async move {
            let message = self.complete(input).await?;
            let mut chunks = Vec::new();
            if let Some(reasoning) = message.reasoning {
                chunks.push(ProviderChunk::Reasoning {
                    text: reasoning,
                    signature: message.reasoning_signature.clone(),
                });
            }
            chunks.extend(
                message
                    .reasoning_state
                    .into_iter()
                    .filter(|state| Some(state) != message.reasoning_signature.as_ref())
                    .map(|signature| ProviderChunk::Reasoning {
                        text: String::new(),
                        signature: Some(signature),
                    }),
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
            Ok(ProviderStream {
                correlation_id: message.correlation_id,
                chunks: Box::pin(futures_util::stream::iter(chunks.into_iter().map(Ok))),
            })
        })
    }
}

impl<T> CompletionProvider for ProviderBackend<T>
where
    T: ProviderTransport,
{
    fn complete<'a>(&'a self, input: &'a ProviderInput) -> ProviderFuture<'a> {
        Box::pin(ProviderBackend::complete(self, input))
    }

    fn stream<'a>(&'a self, input: &'a ProviderInput) -> ProviderStreamFuture<'a> {
        Box::pin(ProviderBackend::stream(self, input))
    }

    fn stream_observed<'a>(
        &'a self,
        input: &'a ProviderInput,
        retries: &'a (dyn RetrySink + 'a),
    ) -> ProviderStreamFuture<'a> {
        Box::pin(ProviderBackend::stream_observed(self, input, retries))
    }

    fn model(&self) -> Option<&str> {
        Some(ProviderBackend::model(self))
    }

    fn call_descriptor(&self) -> Option<ModelCallDescriptor> {
        Some(ProviderBackend::call_descriptor(self))
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
        retries: &'a (dyn RetrySink + 'a),
    ) -> ProviderStreamFuture<'a> {
        (**self).stream_observed(input, retries)
    }

    fn model(&self) -> Option<&str> {
        (**self).model()
    }

    fn call_descriptor(&self) -> Option<ModelCallDescriptor> {
        (**self).call_descriptor()
    }
}

/// Forwards a retry to the turn that is waiting on the request.
pub(super) struct ChannelRetrySink {
    pub(super) reasons: mpsc::UnboundedSender<String>,
}

impl RetrySink for ChannelRetrySink {
    fn retrying(&self, reason: &str) {
        // A closed receiver means the turn is gone; the request still finishes.
        let _ = self.reasons.send(reason.to_owned());
    }
}

pub trait ToolExecutor: Send + Sync {
    fn execute<'a>(&'a self, name: &'a str, arguments: &'a str) -> ToolFuture<'a>;

    fn execute_stream<'a>(
        &'a self,
        name: &'a str,
        arguments: &'a str,
        _output: ToolStreamSink,
    ) -> ToolFuture<'a> {
        self.execute(name, arguments)
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
            self.store
                .update_metadata(&metadata)
                .map_err(|error| error.to_string())
        })
    }
}

//! One model call: the request it reports, and the completion it streams.

use futures_util::StreamExt;
use thiserror::Error;
use tokio::sync::mpsc;

use super::contracts::ChannelRetrySink;
use super::controls::{CancellationToken, Steering, steer_arrived};
use super::ledger::TurnRecorder;
use super::{
    Compactor, CompletionProvider, ConversationEngine, EngineError, ToolExecutor, TranscriptSink,
};
use crate::events::{EngineEvent, ModelMessage};
use crate::provider::{
    AssistantMessage, ModelCallDescriptor, ProviderChunk, ProviderError, ProviderInput,
    aggregate_provider_chunks,
};
use crate::telemetry::{TelemetryCallType, TelemetryContext};
use crate::tracing::{
    BackendFailure, ModelCallSpan, TracedError, model_call_span, set_model_call_response_metadata,
    set_model_call_usage,
};

/// Replaces the request's `metadata` with the census it reports, when the
/// session builds one. Reference `_build_backend_metadata`, dumped without its
/// absent fields; every value it carries is a string.
pub(super) fn stamp_census(
    census: Option<&TelemetryContext>,
    input: &mut ProviderInput,
    session_id: &str,
    call_type: TelemetryCallType,
    message_id: &Option<String>,
) {
    let Some(census) = census else {
        return;
    };
    input.metadata = census
        .request_metadata(Some(session_id), call_type, message_id.clone())
        .properties()
        .into_iter()
        .filter_map(|(key, value)| match value {
            serde_json::Value::String(value) => Some((key, value)),
            _ => None,
        })
        .collect();
}

/// The report of the request `input` is about to make, which is what the
/// reference's agent loop hands its telemetry client one call earlier
/// (`send_request_sent`), a turn's own requests and a compaction's alike.
pub(super) fn request_sent(
    input: &ProviderInput,
    model: String,
    agent_profile: String,
    message_id: Option<String>,
    call_type: TelemetryCallType,
) -> EngineEvent {
    EngineEvent::RequestSent {
        model,
        agent_profile,
        nb_context_chars: input
            .messages
            .iter()
            .map(|message| message.content().chars().count() as u64)
            .sum(),
        nb_context_messages: input.messages.len() as u64,
        nb_prompt_chars: prompt_chars(&input.messages),
        nb_images: input.images.len() as u64,
        // Every provider this port ships accepts images on the request it
        // builds; a backend that refuses one refuses it at the wire, which is
        // not a telemetry decision.
        supports_images: true,
        message_id,
        call_type,
    }
}

/// The size of the operator's latest message among those a request sends,
/// zero when it sends none, as after a compaction whose envelope replaced it.
/// Reference `send_request_sent(nb_prompt_chars=...)` over the last user message
/// that is not injected.
fn prompt_chars(messages: &[ModelMessage]) -> u64 {
    messages
        .iter()
        .rev()
        .find_map(|message| match message {
            ModelMessage::User {
                content,
                injected: false,
                ..
            } => Some(content.chars().count() as u64),
            _ => None,
        })
        .unwrap_or_default()
}

/// A retry as the turn publishes it.
fn retrying(reason: crate::llm::retry::RetryReason) -> EngineEvent {
    EngineEvent::Retrying {
        category: reason.category.as_str().to_owned(),
        detail: reason.detail,
    }
}

impl<P, T, C, S> ConversationEngine<P, T, C, S>
where
    P: CompletionProvider,
    T: ToolExecutor,
    C: Compactor,
    S: TranscriptSink,
{
    /// Streams one completion under the span that reports it. Reference opens
    /// `model_call_span` inside its backend; here the turn opens it and the
    /// backend fills in the HTTP status from inside, which is the same span
    /// either way because the request is polled under it.
    pub(super) async fn stream_completion(
        &self,
        recorder: &mut TurnRecorder<'_>,
        input: &ProviderInput,
        cancellation: &CancellationToken,
        steering: Option<Steering<'_>>,
    ) -> Result<StreamOutcome, EngineError> {
        let descriptor = self
            .provider
            .call_descriptor()
            .unwrap_or(ModelCallDescriptor {
                provider_name: String::new(),
                api_style: String::new(),
                endpoint: String::new(),
            });
        let model = self.resolved_model(input).unwrap_or_default();
        let span = ModelCallSpan {
            provider_name: &descriptor.provider_name,
            provider_api_style: &descriptor.api_style,
            model: &model,
            streaming: input.stream,
            temperature: input
                .limits
                .temperature_millis
                .map(|thousandths| f64::from(thousandths) / 1000.0),
            max_tokens: input.limits.max_tokens.map(i64::from),
            session_id: None,
            call_type: input.metadata.get("call_type").map(String::as_str),
            message_id: input.metadata.get("message_id").map(String::as_str),
            http_method: None,
            http_url: Some(&descriptor.endpoint),
        };
        let call = async {
            match self
                .stream_traced_completion(recorder, input, cancellation, &model, steering)
                .await
            {
                // A backend that refused is a failing span and an answered
                // turn: the outcome the caller reads is unchanged, and the
                // span carries the refusal it would otherwise never see.
                Ok(StreamOutcome::Completed(Err(error))) => Err(ModelCallFailure::Provider(error)),
                Ok(outcome) => Ok(outcome),
                Err(error) => Err(ModelCallFailure::Engine(error)),
            }
        };
        let outcome = if self.provider.traces_model_calls() {
            model_call_span(span, call).await
        } else {
            call.await
        };
        match outcome {
            Ok(outcome) => Ok(outcome),
            Err(ModelCallFailure::Provider(error)) => Ok(StreamOutcome::Completed(Err(error))),
            Err(ModelCallFailure::Engine(error)) => Err(error),
        }
    }

    async fn stream_traced_completion(
        &self,
        recorder: &mut TurnRecorder<'_>,
        input: &ProviderInput,
        cancellation: &CancellationToken,
        model: &str,
        mut steering: Option<Steering<'_>>,
    ) -> Result<StreamOutcome, EngineError> {
        // Retries are reported while the request is still waiting: a client
        // renders the wait, so learning about it once the backend gave up would
        // be too late to be worth anything.
        let (retry_sender, mut retry_reasons) = mpsc::unbounded_channel();
        let retries = ChannelRetrySink {
            reasons: retry_sender,
        };
        let mut opening = self.provider.stream_observed(input, &retries);
        let mut stream = loop {
            tokio::select! {
                result = &mut opening => break match result {
                    Ok(stream) => stream,
                    Err(error) => return Ok(StreamOutcome::Completed(Err(error))),
                },
                Some(reason) = retry_reasons.recv() => {
                    recorder.emit(retrying(reason))?;
                }
                () = cancellation.cancelled() => return Ok(StreamOutcome::Cancelled),
            }
        };
        drop(opening);
        while let Ok(reason) = retry_reasons.try_recv() {
            recorder.emit(retrying(reason))?;
        }
        let (text_id, reasoning_id) = recorder.open_model_call();
        let mut chunks = Vec::new();
        loop {
            let next = tokio::select! {
                next = stream.chunks.next() => next,
                // Reference `inject_user_context`: a steer joins the transcript
                // as it arrives, ahead of the reply still streaming.
                () = steer_arrived(steering.as_ref()) => {
                    let Some(steering) = steering.as_mut() else {
                        continue;
                    };
                    for (content, inject_implicit) in steering.controls.take_steers() {
                        if let Some(message) = self
                            .apply_steer(recorder, steering.messages, content, inject_implicit, cancellation)
                            .await?
                        {
                            return Ok(StreamOutcome::SteerFailed(message));
                        }
                    }
                    continue;
                }
                () = cancellation.cancelled() => return Ok(StreamOutcome::Cancelled),
            };
            let Some(chunk) = next else {
                break;
            };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => return Ok(StreamOutcome::Completed(Err(error))),
            };
            match &chunk {
                ProviderChunk::Text { text } if !text.is_empty() => {
                    recorder.emit(EngineEvent::ModelText {
                        text: text.clone(),
                        message_id: Some(text_id.clone()),
                    })?;
                }
                ProviderChunk::Reasoning { text, .. } if !text.is_empty() => {
                    recorder.emit(EngineEvent::ModelReasoning {
                        text: text.clone(),
                        message_id: Some(reasoning_id.clone()),
                    })?;
                }
                ProviderChunk::Text { .. }
                | ProviderChunk::Reasoning { .. }
                | ProviderChunk::ReasoningPayload { .. }
                | ProviderChunk::ToolCall { .. }
                | ProviderChunk::Usage { .. }
                | ProviderChunk::Refusal { .. }
                | ProviderChunk::Stop { .. } => {}
            }
            chunks.push(chunk);
        }
        if chunks.is_empty() {
            return Ok(StreamOutcome::Completed(Err(
                ProviderError::MalformedStream(
                    "provider stream produced no typed chunks".to_owned(),
                ),
            )));
        }
        let aggregated = aggregate_provider_chunks(chunks, stream.correlation_id);
        if let Ok(message) = &aggregated {
            // Reference `set_model_call_usage` and
            // `set_model_call_response_metadata`, both read off the answered
            // response rather than off the request.
            set_model_call_usage(message.usage.input_tokens, message.usage.output_tokens);
            set_model_call_response_metadata(&serde_json::json!({
                "model": model,
                "id": message.correlation_id,
                "choices": [{"finish_reason": message.stop_reason}],
            }));
        }
        Ok(StreamOutcome::Completed(aggregated.map(Box::new)))
    }
}

/// One provider exchange: either it produced a response, or the turn was cancelled.
pub(super) enum StreamOutcome {
    /// The assistant message is boxed: it dwarfs the cancelled variant.
    Completed(Result<Box<AssistantMessage>, ProviderError>),
    Cancelled,
    /// A steer taken while the reply streamed failed the turn reading a file
    /// it mentioned.
    SteerFailed(String),
}

/// What one model call can fail with, so the span reads a refusal as a failure
/// while the turn still reads it as an answer.
#[derive(Debug, Error)]
enum ModelCallFailure {
    #[error(transparent)]
    Provider(ProviderError),
    #[error(transparent)]
    Engine(EngineError),
}

impl TracedError for ModelCallFailure {
    fn error_type(&self) -> &'static str {
        match self {
            Self::Provider(error) => error.error_type(),
            Self::Engine(error) => error.error_type(),
        }
    }

    fn backend_failure(&self) -> Option<BackendFailure> {
        match self {
            Self::Provider(error) => error.backend_failure(),
            Self::Engine(error) => error.backend_failure(),
        }
    }
}

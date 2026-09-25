//! The engine's completion contract, answered by a [`Backend`].
//!
//! The conversation engine asks for a completion with a
//! [`ProviderInput`] and reads [`ProviderChunk`]s back. This adapter turns the
//! input into the reference's call (`_chat_streaming` and `_complete` in
//! `vibe/core/agent_loop/_loop.py`): the history as `LLMMessage`s, reordered
//! so every tool call is followed by its results, with the provider's headers,
//! the session affinity and the model's thinking level; then it folds the
//! backend's pieces through [`super::call`], so an incomplete stream, an answer
//! without usage and a refusal fail the call the way the reference raises them,
//! and every failure reaches the engine already classified.

use std::collections::VecDeque;

use futures_util::StreamExt;
use serde_json::{Map, Value};

use super::call::{self, Answered, Failed, StreamingCall};
use super::error::{LocalFailure, LocalKind};
use super::pump::ChunkStream;
use super::retry::RetryObserver;
use super::types::{Chunk, Image, Message, Role, Tool, ToolCall, ToolChoice};
use super::{Backend, BackendContext, ModelRequest};
use crate::engine::{CompletionProvider, ProviderFuture, ProviderStreamFuture};
use crate::events::{ModelMessage, ModelToolCall};
use crate::provider::config::{BackendKind, ModelConfig, ProviderConfig};
use crate::provider::{
    AssistantMessage, CallError, ModelCallDescriptor, ProviderChunk, ProviderError, ProviderInput,
    ProviderStream, Usage,
};

/// The thinking levels a model entry and a session may name.
const THINKING_LEVELS: &[&str] = &["off", "low", "medium", "high", "max"];

/// A provider entry, the models it serves, and the backend that reaches it.
pub struct LlmCompletion {
    provider: ProviderConfig,
    models: Vec<ModelConfig>,
    default_model: String,
    backend: Backend,
}

impl LlmCompletion {
    /// # Errors
    ///
    /// A provider address that would send its key in the clear to another
    /// host, or carries credentials of its own, and a provider entry the
    /// backend cannot run with.
    pub fn new(
        provider: ProviderConfig,
        models: Vec<ModelConfig>,
        default_model: impl Into<String>,
        context: BackendContext,
    ) -> Result<Self, LocalFailure> {
        check_address(&provider)?;
        let backend = Backend::new(provider.clone(), context)?;
        Ok(Self {
            provider,
            models,
            default_model: default_model.into(),
            backend,
        })
    }

    /// The model a call runs on: the configured entry the name matches by
    /// alias then by API name, or a bare entry for a name none declares.
    fn model_for(&self, input: &ProviderInput) -> ModelConfig {
        let name = input
            .model_override
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or(&self.default_model);
        let mut model = self
            .models
            .iter()
            .find(|model| model.alias == name)
            .or_else(|| self.models.iter().find(|model| model.name == name))
            .cloned()
            .unwrap_or_else(|| ModelConfig::new(name, self.provider.name.clone()));
        if let Some(temperature) = input.limits.temperature_millis {
            model.temperature = f64::from(temperature) / 1000.0;
        }
        let requested = input
            .reasoning_effort
            .as_deref()
            .filter(|level| THINKING_LEVELS.contains(level));
        if let Some(level) = requested {
            model.thinking = level.to_owned();
        } else if input.thinking && model.thinking == "off" {
            model.thinking = "medium".to_owned();
        }
        model
    }
}

/// Everything one request is made of, owned so the request can borrow it.
struct Prepared {
    model: ModelConfig,
    messages: Vec<Message>,
    tools: Vec<Tool>,
    tool_choice: Option<ToolChoice>,
    headers: Vec<(String, String)>,
    metadata: Option<Map<String, Value>>,
    max_tokens: Option<u64>,
}

impl Prepared {
    fn new(completion: &LlmCompletion, input: &ProviderInput) -> Self {
        let model = completion.model_for(input);
        let history = history(input);
        let messages = call::messages_for_backend(history, model.supports_images);
        let tools: Vec<Tool> = input
            .tools
            .iter()
            .map(|tool| Tool {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.input_schema.clone(),
            })
            .collect();
        let tool_choice = input.tool_choice.as_ref().map(|choice| match choice {
            crate::provider::ToolChoice::Auto => ToolChoice::Auto,
            crate::provider::ToolChoice::None => ToolChoice::None,
            crate::provider::ToolChoice::Required => ToolChoice::Required,
            crate::provider::ToolChoice::Tool { name } => ToolChoice::Tool(
                tools
                    .iter()
                    .find(|tool| &tool.name == name)
                    .cloned()
                    .unwrap_or_else(|| Tool {
                        name: name.clone(),
                        description: String::new(),
                        parameters: Value::Object(Map::new()),
                    }),
            ),
        });
        // A session's call carries the provider's headers and the session
        // affinity (`_get_extra_headers`); a utility completion runs outside
        // any session and sends the user agent alone (`run_utility_completion`).
        let mut headers = match input.session_id.as_deref() {
            Some(session_id) => call::extra_headers(&completion.provider, session_id),
            None => vec![(
                "user-agent".to_owned(),
                call::user_agent(completion.provider.backend),
            )],
        };
        for (name, value) in &input.headers {
            match headers
                .iter_mut()
                .find(|(known, _)| known.eq_ignore_ascii_case(name))
            {
                Some(existing) => existing.1.clone_from(value),
                None => headers.push((name.clone(), value.clone())),
            }
        }
        let metadata = (!input.metadata.is_empty()).then(|| {
            input
                .metadata
                .iter()
                .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                .collect()
        });
        Self {
            model,
            messages,
            tools,
            tool_choice,
            headers,
            metadata,
            max_tokens: input.limits.max_tokens.map(u64::from),
        }
    }

    fn request(&self) -> ModelRequest<'_> {
        ModelRequest {
            model: &self.model,
            messages: &self.messages,
            temperature: self.model.temperature,
            tools: (!self.tools.is_empty()).then_some(self.tools.as_slice()),
            max_tokens: self.max_tokens,
            tool_choice: self.tool_choice.as_ref(),
            extra_headers: &self.headers,
            metadata: self.metadata.as_ref(),
        }
    }
}

/// The engine's history as the reference's messages. The images of the
/// turn ride on its last user message, and a tool result names the tool its
/// call asked for.
fn history(input: &ProviderInput) -> Vec<Message> {
    let mut messages: Vec<Message> = Vec::with_capacity(input.messages.len());
    for message in &input.messages {
        let converted = match message {
            ModelMessage::System { content } => Message::system(content.clone()),
            ModelMessage::User {
                content, injected, ..
            } => {
                let mut user = Message::user(content.clone());
                user.injected = *injected;
                user
            }
            ModelMessage::Assistant {
                content,
                reasoning,
                reasoning_payloads,
                tool_calls,
                ..
            } => {
                let mut assistant = Message::assistant();
                assistant.content = (!content.is_empty()).then(|| content.clone());
                assistant.reasoning_content = reasoning.clone();
                assistant.reasoning_payloads =
                    (!reasoning_payloads.is_empty()).then(|| reasoning_payloads.clone());
                assistant.tool_calls = (!tool_calls.is_empty()).then(|| {
                    tool_calls
                        .iter()
                        .enumerate()
                        .map(|(index, call)| ToolCall {
                            id: Some(call.id.clone()),
                            index: u64::try_from(index).ok(),
                            name: Some(call.name.clone()),
                            arguments: Some(call.arguments.clone()),
                        })
                        .collect()
                });
                assistant
            }
            ModelMessage::Tool {
                call_id, content, ..
            } => {
                let mut tool = Message::new(Role::Tool).with_content(content.clone());
                tool.tool_call_id = Some(call_id.clone());
                tool.name = messages.iter().rev().find_map(|earlier| {
                    earlier
                        .tool_calls
                        .iter()
                        .flatten()
                        .find(|call| call.id.as_deref() == Some(call_id.as_str()))
                        .and_then(|call| call.name.clone())
                });
                tool
            }
        };
        messages.push(converted);
    }
    if !input.images.is_empty()
        && let Some(last_user) = messages
            .iter_mut()
            .rev()
            .find(|message| message.role == Role::User)
    {
        last_user.images = input
            .images
            .iter()
            .map(|image| Image {
                mime_type: image.media_type.clone(),
                data: image.data.clone(),
            })
            .collect();
    }
    messages
}

/// The URL a streamed call to `model` is sent to.
fn call_url(provider: &ProviderConfig, model: &str) -> String {
    let base = provider.api_base.as_str();
    match (provider.backend, provider.api_style.as_str()) {
        (BackendKind::Mistral, _) => format!(
            "{}/v1/chat/completions",
            super::mistral::server_url(base).unwrap_or_else(|| base.to_owned())
        ),
        (BackendKind::Generic, "anthropic") => format!("{base}/v1/messages"),
        (BackendKind::Generic, "openai-responses") => format!("{base}/responses"),
        (BackendKind::Generic, "vertex-anthropic") => {
            super::vertex::base_url(&provider.region)
                + &super::vertex::endpoint(&provider.region, &provider.project_id, model, true)
        }
        (BackendKind::Generic, _) => format!("{base}/chat/completions"),
    }
}

fn check_address(provider: &ProviderConfig) -> Result<(), LocalFailure> {
    // A Vertex provider is addressed by its region, not by `api_base`.
    if provider.backend == BackendKind::Generic && provider.api_style == "vertex-anthropic" {
        return Ok(());
    }
    let refuse = |reason: String| LocalFailure::new(LocalKind::Configuration, reason);
    let parsed = url::Url::parse(&provider.api_base)
        .map_err(|error| refuse(format!("the provider address is not a URL: {error}")))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(refuse(
            "the provider address must not carry credentials".to_owned(),
        ));
    }
    if !crate::text::is_secure_transport(&parsed) {
        return Err(refuse(
            "the provider address must use HTTPS unless it is a loopback host".to_owned(),
        ));
    }
    Ok(())
}

fn assistant_message(message: &Message, usage: Option<super::types::Usage>) -> AssistantMessage {
    AssistantMessage {
        text: message.content.clone().unwrap_or_default(),
        reasoning: message
            .reasoning_content
            .clone()
            .filter(|reasoning| !reasoning.is_empty()),
        reasoning_payloads: message.reasoning_payloads.clone().unwrap_or_default(),
        tool_calls: model_tool_calls(message),
        usage: usage.map(engine_usage).unwrap_or_default(),
        refusal: None,
        stop_reason: String::new(),
        correlation_id: None,
    }
}

fn model_tool_calls(message: &Message) -> Vec<ModelToolCall> {
    message
        .tool_calls
        .iter()
        .flatten()
        .map(|call| ModelToolCall {
            id: call.id.clone().unwrap_or_default(),
            name: call.name.clone().unwrap_or_default(),
            arguments: call.arguments.clone().unwrap_or_default(),
        })
        .collect()
}

const fn engine_usage(usage: super::types::Usage) -> Usage {
    Usage {
        input_tokens: usage.prompt_tokens,
        output_tokens: usage.completion_tokens,
    }
}

fn call_error(failed: Failed) -> ProviderError {
    let appended = failed
        .appended
        .as_ref()
        .map(|message| assistant_message(message, failed.usage));
    ProviderError::Call(Box::new(CallError {
        failure: failed.failure,
        appended,
    }))
}

/// What a piece of the answer publishes while it streams.
fn deltas(piece: &Chunk) -> impl Iterator<Item = ProviderChunk> {
    let reasoning = piece
        .message
        .reasoning_content
        .clone()
        .filter(|text| !text.is_empty())
        .map(|text| ProviderChunk::Reasoning { text });
    let text = piece
        .message
        .content
        .clone()
        .filter(|text| !text.is_empty())
        .map(|text| ProviderChunk::Text { text });
    reasoning.into_iter().chain(text)
}

/// What an answered call adds once it is whole: the reasoning items, the tool
/// calls, the usage and why it stopped.
fn closing(answered: &Answered) -> Vec<ProviderChunk> {
    let message = answered.message();
    let mut chunks: Vec<ProviderChunk> = message
        .reasoning_payloads
        .iter()
        .flatten()
        .map(|payload| ProviderChunk::ReasoningPayload {
            payload: payload.clone(),
        })
        .collect();
    chunks.extend(
        model_tool_calls(message)
            .into_iter()
            .map(|call| ProviderChunk::ToolCall {
                id: call.id,
                name: call.name,
                arguments: call.arguments,
            }),
    );
    chunks.push(ProviderChunk::Usage {
        input_tokens: answered.usage.prompt_tokens,
        output_tokens: answered.usage.completion_tokens,
    });
    chunks.push(ProviderChunk::Stop {
        reason: answered
            .chunk
            .stop
            .as_ref()
            .and_then(|stop| stop.reason.clone())
            .unwrap_or_else(|| "stop".to_owned()),
    });
    chunks
}

struct Folding<'a> {
    pieces: ChunkStream<'a>,
    call: Option<StreamingCall>,
    ready: VecDeque<Result<ProviderChunk, ProviderError>>,
}

impl Folding<'_> {
    /// Reads the backend until something is ready to publish, or the call
    /// ended.
    async fn advance(&mut self) {
        while self.ready.is_empty() {
            let Some(mut call) = self.call.take() else {
                return;
            };
            match self.pieces.next().await {
                Some(Ok(piece)) => match call.push(piece) {
                    Ok(piece) => {
                        self.ready.extend(deltas(&piece).map(Ok));
                        self.call = Some(call);
                    }
                    Err(failed) => self.ready.push_back(Err(call_error(*failed))),
                },
                Some(Err(failure)) => self.ready.push_back(Err(call_error(call.fail(failure)))),
                None => match call.finish() {
                    Ok(answered) => self.ready.extend(closing(&answered).into_iter().map(Ok)),
                    Err(failed) => self.ready.push_back(Err(call_error(*failed))),
                },
            }
        }
    }
}

impl LlmCompletion {
    async fn streamed<'a>(
        &'a self,
        input: &ProviderInput,
        retries: &dyn RetryObserver,
    ) -> Result<ProviderStream<'a>, ProviderError> {
        let prepared = Prepared::new(self, input);
        let call = StreamingCall::new(&self.provider, &prepared.model.name);
        let pieces = match self
            .backend
            .complete_streaming(&prepared.request(), retries)
            .await
        {
            Ok(pieces) => pieces,
            Err(failure) => return Err(call_error(call.fail(failure))),
        };
        let mut folding = Folding {
            pieces,
            call: Some(call),
            ready: VecDeque::new(),
        };
        // The first piece carries the provider's identifier for the call, which
        // the engine reads when the stream opens.
        folding.advance().await;
        let correlation_id = folding
            .call
            .as_ref()
            .and_then(|call| call.correlation_id().map(str::to_owned));
        let chunks = futures_util::stream::unfold(folding, |mut folding| async move {
            folding.advance().await;
            let next = folding.ready.pop_front()?;
            Some((next, folding))
        });
        Ok(ProviderStream {
            correlation_id,
            chunks: Box::pin(chunks),
        })
    }

    async fn completed(
        &self,
        input: &ProviderInput,
        retries: &dyn RetryObserver,
    ) -> Result<AssistantMessage, ProviderError> {
        let prepared = Prepared::new(self, input);
        let result = self.backend.complete(&prepared.request(), retries).await;
        let answered = call::finish_complete(result, &self.provider, &prepared.model.name)
            .map_err(|failed| call_error(*failed))?;
        let mut message = assistant_message(answered.message(), Some(answered.usage));
        message.stop_reason = answered
            .chunk
            .stop
            .as_ref()
            .and_then(|stop| stop.reason.clone())
            .unwrap_or_else(|| "stop".to_owned());
        message.correlation_id = answered.correlation_id;
        Ok(message)
    }
}

impl CompletionProvider for LlmCompletion {
    fn complete<'a>(&'a self, input: &'a ProviderInput) -> ProviderFuture<'a> {
        Box::pin(self.completed(input, &super::retry::NoRetryObserver))
    }

    fn stream_observed<'a>(
        &'a self,
        input: &'a ProviderInput,
        retries: &'a (dyn RetryObserver + 'a),
    ) -> ProviderStreamFuture<'a> {
        Box::pin(async move {
            if input.stream {
                return self.streamed(input, retries).await;
            }
            let message = self.completed(input, retries).await?;
            Ok(crate::engine::chunks_of(message))
        })
    }

    fn stream<'a>(&'a self, input: &'a ProviderInput) -> ProviderStreamFuture<'a> {
        self.stream_observed(input, &super::retry::NoRetryObserver)
    }

    fn model(&self) -> Option<&str> {
        Some(&self.default_model)
    }

    fn call_descriptor(&self) -> Option<ModelCallDescriptor> {
        Some(ModelCallDescriptor {
            provider_name: self.provider.name.clone(),
            api_style: self.provider.api_style.clone(),
            endpoint: call_url(&self.provider, &self.default_model),
        })
    }

    fn traces_model_calls(&self) -> bool {
        !matches!(self.backend, Backend::Mistral(_))
    }
}

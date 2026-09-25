//! One model call as the agent loop makes it.
//!
//! Reference `_chat_streaming`, `_chat` and `_complete`
//! (`vibe/core/agent_loop/_loop.py`). Before a request: tool results are moved
//! next to the call they answer, images are dropped for a model that cannot
//! read them, and the provider's headers go out with the user agent and the
//! session affinity. While a stream runs, each piece is reduced to what the
//! loop keeps and folded into the answer. After it: a stream that never said
//! why it ended is incomplete (unless the provider never says), an answer
//! without usage is refused, and a refusal is appended before it fails the
//! turn. A failure is classified into what the turn publishes; a partial
//! answer interrupted by the backend keeps its text in the history.

use std::collections::BTreeSet;

use super::adapter::processed;
use super::error::{BackendFailure, CallFailure, LocalFailure, LocalKind, WrappedCause};
use super::types::{Chunk, Message, Role, Usage};
use crate::provider::config::{BackendKind, ProviderConfig};

/// Moves whatever fell between a tool call and its results to just after
/// them, so every call is followed by its answers. Reference
/// `reorder_for_tool_adjacency` (`vibe/core/compaction/context.py`).
#[must_use]
pub fn reorder_for_tool_adjacency(messages: Vec<Message>) -> Vec<Message> {
    let mut ordered = Vec::with_capacity(messages.len());
    let mut remaining = messages.into_iter();
    while let Some(message) = remaining.next() {
        let pending: BTreeSet<String> = if message.role == Role::Assistant {
            message
                .tool_calls
                .iter()
                .flatten()
                .filter_map(|call| call.id.clone())
                .collect()
        } else {
            BTreeSet::new()
        };
        ordered.push(message);
        if pending.is_empty() {
            continue;
        }
        let mut pending = pending;
        let mut displaced = Vec::new();
        while !pending.is_empty() {
            let Some(following) = remaining.next() else {
                break;
            };
            let answers = following.role == Role::Tool
                && following
                    .tool_call_id
                    .as_ref()
                    .is_some_and(|id| pending.contains(id));
            if answers {
                if let Some(id) = &following.tool_call_id {
                    pending.remove(id);
                }
                ordered.push(following);
            } else {
                displaced.push(following);
            }
        }
        ordered.extend(reorder_for_tool_adjacency(displaced));
    }
    ordered
}

/// The conversation a backend is sent. Reference `_messages_for_backend`,
/// applied to a history the caller already cut at its latest compaction.
#[must_use]
pub fn messages_for_backend(messages: Vec<Message>, supports_images: bool) -> Vec<Message> {
    let mut messages = reorder_for_tool_adjacency(messages);
    if !supports_images {
        for message in &mut messages {
            message.images.clear();
        }
    }
    messages
}

/// `Mistral-Vibe/<version>`, prefixed the way the Mistral client prefixes its
/// own. Reference `get_user_agent` (`vibe/utils/http.py`).
#[must_use]
pub fn user_agent(backend: BackendKind) -> String {
    let agent = format!("Mistral-Vibe/{}", env!("CARGO_PKG_VERSION"));
    match backend {
        BackendKind::Mistral => format!("mistral-client-python/{agent}"),
        BackendKind::Generic => agent,
    }
}

/// The headers every call adds: the provider's own, the user agent and the
/// session affinity. Reference `_get_extra_headers`.
#[must_use]
pub fn extra_headers(provider: &ProviderConfig, session_id: &str) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = provider
        .extra_headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    set_header(&mut headers, "user-agent", user_agent(provider.backend));
    set_header(&mut headers, "x-affinity", session_id.to_owned());
    headers
}

fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    match headers.iter_mut().find(|(known, _)| known == name) {
        Some(existing) => existing.1 = value,
        None => headers.push((name.to_owned(), value)),
    }
}

/// What an answered call leaves behind.
#[derive(Debug, Clone, PartialEq)]
pub struct Answered {
    /// The answer, folded from its pieces.
    pub chunk: Chunk,
    /// The usage the loop's statistics add up.
    pub usage: Usage,
    /// The provider's identifier for the call, which the loop hands its
    /// telemetry rather than keeping on the answer.
    pub correlation_id: Option<String>,
}

impl Answered {
    /// The message the loop appends to its history.
    #[must_use]
    pub fn message(&self) -> &Message {
        &self.chunk.message
    }
}

/// What a failed call leaves behind.
#[derive(Debug, Clone, PartialEq)]
pub struct Failed {
    pub failure: CallFailure,
    /// The message the loop still appends: a refused answer in full, or the
    /// text of an answer the backend interrupted.
    pub appended: Option<Message>,
    /// The usage the statistics still count, which only a refusal has.
    pub usage: Option<Usage>,
    /// The answer as far as it got.
    pub partial: Option<Chunk>,
}

/// Names a backend failure as the loop re-raises it.
#[must_use]
pub fn classify(failure: BackendFailure, provider: &str, model: &str) -> CallFailure {
    let wrap = |cause| CallFailure::Wrapped {
        provider: provider.to_owned(),
        model: model.to_owned(),
        cause,
    };
    match failure {
        BackendFailure::Backend(error) => {
            if error.status == Some(429) {
                CallFailure::RateLimit {
                    provider: provider.to_owned(),
                    model: model.to_owned(),
                    cause: error,
                }
            } else if error.is_context_too_long() {
                CallFailure::ContextTooLong {
                    provider: provider.to_owned(),
                    model: model.to_owned(),
                    cause: error,
                }
            } else if error.is_response_too_long() {
                CallFailure::ResponseTooLong {
                    provider: provider.to_owned(),
                    model: model.to_owned(),
                    cause: error,
                }
            } else if error.is_invalid_model() {
                CallFailure::InvalidModel(error)
            } else {
                wrap(WrappedCause::Backend(error))
            }
        }
        BackendFailure::Local(local) => wrap(WrappedCause::Local(local)),
        BackendFailure::ResponsesStream(error) => wrap(WrappedCause::Local(LocalFailure::new(
            LocalKind::Runtime,
            error.message,
        ))),
    }
}

/// The text of an interrupted answer, which is all the history keeps of it.
fn interrupted(partial: Option<&Chunk>) -> Option<Message> {
    let content = partial?.message.content.as_deref()?;
    (!content.is_empty()).then(|| Message::assistant().with_content(content))
}

fn refusal(chunk: &Chunk, provider: &str, model: &str) -> Option<CallFailure> {
    let stop = chunk.stop.as_ref().filter(|stop| stop.is_refusal())?;
    Some(CallFailure::Refusal {
        provider: provider.to_owned(),
        model: model.to_owned(),
        category: stop.category.clone(),
        explanation: stop.explanation.clone(),
    })
}

/// Folds a streamed answer the way `_chat_streaming` does.
#[derive(Debug)]
pub struct StreamingCall {
    provider: String,
    model: String,
    emits_finish_reason: bool,
    aggregate: Option<Chunk>,
    usage: Usage,
    correlation_id: Option<String>,
}

impl StreamingCall {
    #[must_use]
    pub fn new(provider: &ProviderConfig, model: &str) -> Self {
        Self {
            provider: provider.name.clone(),
            model: model.to_owned(),
            emits_finish_reason: provider.emits_finish_reason,
            aggregate: None,
            usage: Usage::default(),
            correlation_id: None,
        }
    }

    /// The last correlation identifier a piece carried.
    #[must_use]
    pub fn correlation_id(&self) -> Option<&str> {
        self.correlation_id.as_deref()
    }

    /// Folds one piece in and returns it as the loop publishes it.
    ///
    /// # Errors
    ///
    /// A piece that cannot be folded into the answer so far.
    pub fn push(&mut self, chunk: Chunk) -> Result<Chunk, Box<Failed>> {
        if let Some(correlation) = chunk.correlation_id.clone() {
            self.correlation_id = Some(correlation);
        }
        self.usage = self.usage + chunk.usage.unwrap_or_default();
        let piece = Chunk {
            message: processed(chunk.message),
            usage: chunk.usage,
            correlation_id: None,
            stop: chunk.stop,
        };
        let folded = match self.aggregate.take() {
            None => piece.clone(),
            Some(aggregate) => match aggregate.clone().merge(piece.clone()) {
                Ok(folded) => folded,
                Err(error) => {
                    return Err(Box::new(Failed {
                        failure: CallFailure::Wrapped {
                            provider: self.provider.clone(),
                            model: self.model.clone(),
                            cause: WrappedCause::Local(LocalFailure::new(
                                LocalKind::Value,
                                error.to_string(),
                            )),
                        },
                        appended: None,
                        usage: None,
                        partial: Some(aggregate),
                    }));
                }
            },
        };
        self.aggregate = Some(folded);
        Ok(piece)
    }

    /// The call failed while it streamed.
    #[must_use]
    pub fn fail(self, failure: BackendFailure) -> Failed {
        let appended = match failure {
            BackendFailure::Backend(_) => interrupted(self.aggregate.as_ref()),
            _ => None,
        };
        Failed {
            failure: classify(failure, &self.provider, &self.model),
            appended,
            usage: None,
            partial: self.aggregate,
        }
    }

    /// The stream ended.
    ///
    /// # Errors
    ///
    /// An incomplete stream, an answer without usage, and a refusal.
    pub fn finish(self) -> Result<Answered, Box<Failed>> {
        let emits_finish_reason = self.emits_finish_reason;
        let aggregate = match self.aggregate {
            Some(aggregate) if !(emits_finish_reason && aggregate.stop.is_none()) => aggregate,
            partial => {
                return Err(Box::new(Failed {
                    failure: CallFailure::IncompleteStream {
                        provider: self.provider,
                        model: self.model,
                    },
                    appended: interrupted(partial.as_ref()),
                    usage: None,
                    partial,
                }));
            }
        };
        if aggregate.usage.is_none() {
            return Err(Box::new(Failed {
                failure: CallFailure::Wrapped {
                    provider: self.provider,
                    model: self.model,
                    cause: WrappedCause::MissingUsage { streaming: true },
                },
                appended: None,
                usage: None,
                partial: Some(aggregate),
            }));
        }
        if let Some(failure) = refusal(&aggregate, &self.provider, &self.model) {
            return Err(Box::new(Failed {
                failure,
                appended: Some(aggregate.message.clone()),
                usage: Some(self.usage),
                partial: Some(aggregate),
            }));
        }
        Ok(Answered {
            chunk: aggregate,
            usage: self.usage,
            correlation_id: self.correlation_id,
        })
    }
}

/// Reads a non-streaming call's outcome the way `_complete` and `_chat` do.
///
/// # Errors
///
/// The backend failing, an answer without usage, and a refusal.
pub fn finish_complete(
    result: Result<Chunk, BackendFailure>,
    provider: &ProviderConfig,
    model: &str,
) -> Result<Answered, Box<Failed>> {
    let chunk = result.map_err(|failure| {
        Box::new(Failed {
            failure: classify(failure, &provider.name, model),
            appended: None,
            usage: None,
            partial: None,
        })
    })?;
    let Some(usage) = chunk.usage else {
        return Err(Box::new(Failed {
            failure: CallFailure::Wrapped {
                provider: provider.name.clone(),
                model: model.to_owned(),
                cause: WrappedCause::MissingUsage { streaming: false },
            },
            appended: None,
            usage: None,
            partial: Some(chunk),
        }));
    };
    let correlation_id = chunk.correlation_id;
    let chunk = Chunk {
        message: processed(chunk.message),
        usage: Some(usage),
        correlation_id: None,
        stop: chunk.stop,
    };
    if let Some(failure) = refusal(&chunk, &provider.name, model) {
        return Err(Box::new(Failed {
            failure,
            appended: Some(chunk.message.clone()),
            usage: Some(usage),
            partial: Some(chunk),
        }));
    }
    Ok(Answered {
        chunk,
        usage,
        correlation_id,
    })
}

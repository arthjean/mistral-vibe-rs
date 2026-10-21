//! One streamed model call: its fragments published as they arrive, and the
//! provider retries it waits on.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::time::Instant;
use vibe_core::clock::now_millis;
use vibe_core::provider::{ProviderChunk, ProviderError, ProviderInput};

use super::{SessionCore, Sink, TurnEnvironment};
use crate::state::{Advance, CompletionDelta, TokenUsage};

/// How long streamed fragments gather before they are published together.
const DELTA_INTERVAL: Duration = Duration::from_millis(75);

/// What one model call answered.
#[derive(Debug, Clone, Default)]
pub(super) struct Completion {
    pub(super) text: String,
    pub(super) reasoning: Option<String>,
    /// Each call's ID, tool name and raw arguments.
    pub(super) tool_calls: Vec<(String, String, String)>,
    pub(super) usage: Option<TokenUsage>,
}

/// One model call, its fragments published as they stream in.
pub(super) async fn stream_completion(
    core: &Arc<Mutex<SessionCore>>,
    environment: &TurnEnvironment,
    input: &ProviderInput,
    turn_id: &str,
    action_id: &str,
    sink: &Sink,
) -> Result<Completion, ProviderError> {
    let retries = TurnRetries {
        core,
        sink,
        turn_id,
        attempts: std::sync::atomic::AtomicU32::new(0),
    };
    let mut stream = environment
        .provider
        .stream_observed(input, &retries)
        .await?;
    retries.recovered();
    let mut completion = Completion::default();
    let mut reasoning = String::new();
    let mut pending = CompletionDelta {
        action_id: action_id.to_owned(),
        turn_id: turn_id.to_owned(),
        ..CompletionDelta::default()
    };
    let mut flushed_at: Option<Instant> = None;
    let flush = |pending: &mut CompletionDelta, flushed_at: &mut Option<Instant>| {
        if pending.text.is_empty() && pending.reasoning.is_empty() {
            return;
        }
        if let Ok(mut core) = core.lock() {
            let advance = core.projector.apply_completion_delta(pending, now_millis());
            sink(core.publication(advance, None));
        }
        pending.text.clear();
        pending.reasoning.clear();
        *flushed_at = Some(Instant::now());
    };
    loop {
        let deadline = flushed_at
            .filter(|_| !pending.text.is_empty() || !pending.reasoning.is_empty())
            .map(|at| at + DELTA_INTERVAL);
        let chunk = match deadline {
            Some(deadline) => tokio::select! {
                chunk = stream.chunks.next() => chunk,
                () = tokio::time::sleep_until(deadline) => {
                    flush(&mut pending, &mut flushed_at);
                    continue;
                }
            },
            None => stream.chunks.next().await,
        };
        let Some(chunk) = chunk else {
            break;
        };
        match chunk? {
            ProviderChunk::Text { text } => {
                completion.text.push_str(&text);
                pending.text.push_str(&text);
            }
            ProviderChunk::Reasoning { text } => {
                reasoning.push_str(&text);
                pending.reasoning.push_str(&text);
            }
            ProviderChunk::ToolCall {
                id,
                name,
                arguments,
            } => completion.tool_calls.push((id, name, arguments)),
            ProviderChunk::Usage {
                input_tokens,
                output_tokens,
                cached_tokens,
            } => {
                completion.usage = Some(TokenUsage {
                    input_tokens,
                    output_tokens,
                    total_tokens: input_tokens + output_tokens,
                    cached_input_tokens: cached_tokens,
                });
            }
            ProviderChunk::ReasoningPayload { .. }
            | ProviderChunk::Refusal { .. }
            | ProviderChunk::Stop { .. } => {}
        }
        let due = flushed_at.is_none_or(|at| at.elapsed() >= DELTA_INTERVAL);
        if due {
            flush(&mut pending, &mut flushed_at);
        }
    }
    // What arrived since the last publication is published before the answer
    // commits.
    flush(&mut pending, &mut flushed_at);
    if !reasoning.is_empty() {
        completion.reasoning = Some(reasoning);
    }
    // Reference `_read_stream`: an answer with nothing in it is one space.
    if completion.text.is_empty()
        && completion.reasoning.is_none()
        && completion.tool_calls.is_empty()
    {
        completion.text = " ".to_owned();
    }
    Ok(completion)
}

/// Publishes the provider retries a model call waits on (reference
/// `_set_provider_retry`): each one shows in the state until an attempt is
/// answered or the turn ends.
struct TurnRetries<'a> {
    core: &'a Arc<Mutex<SessionCore>>,
    sink: &'a Sink,
    turn_id: &'a str,
    attempts: std::sync::atomic::AtomicU32,
}

impl TurnRetries<'_> {
    fn publish(&self, retrying: Option<Value>) {
        let Ok(mut core) = self.core.lock() else {
            return;
        };
        if core.projector.state.retrying == retrying {
            return;
        }
        core.projector.state.retrying = retrying;
        (self.sink)(core.publication(Advance::Provisional, None));
    }

    /// An attempt was answered, so nothing is retrying any more.
    fn recovered(&self) {
        if self.attempts.load(std::sync::atomic::Ordering::Relaxed) > 0 {
            self.publish(None);
        }
    }
}

impl vibe_core::llm::retry::RetryObserver for TurnRetries<'_> {
    fn retrying(&self, reason: &vibe_core::llm::retry::RetryReason) {
        self.retrying_after(reason, 0.0);
    }

    fn retrying_after(&self, reason: &vibe_core::llm::retry::RetryReason, delay_seconds: f64) {
        let attempt = self
            .attempts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let delay_millis = (delay_seconds * 1000.0).round().max(0.0) as u64;
        self.publish(Some(json!({
            "turnId": self.turn_id,
            "category": reason.category.as_str(),
            "detail": reason.detail,
            "retryAt": now_millis() + delay_millis,
            "retryAttempt": attempt,
        })));
    }
}

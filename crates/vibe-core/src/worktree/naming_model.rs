//! Asking a small model what to call a worktree.
//!
//! Reference `vibe/core/git/worktree/naming_model.py` over
//! `vibe/core/llm/utility_completion.py`. Naming is a nicety on the
//! session-start path, and the caller always holds a deterministic name, so
//! every failure is [`None`]: no prompt, no provider, no key, too slow, or an
//! error of any kind. The request is bounded twice, one attempt at 1.5 seconds
//! inside a 2 second budget for everything, and never retried.

use std::sync::Arc;
use std::time::Duration;

use crate::engine::CompletionProvider;
use crate::events::ModelMessage;
use crate::llm::completion::LlmCompletion;
use crate::llm::{BackendContext, Credentials, utility};
use crate::observability::{self, LogLevel};
use crate::prompt::library::UtilityPrompt;
use crate::provider::config::{ApiSettings, ModelRouting};
use crate::provider::{ProviderInput, RequestLimits};

/// One attempt's deadline (`naming_model.py:15`).
pub const REQUEST_TIMEOUT: Duration = Duration::from_millis(1_500);
/// Everything around the attempt included (`naming_model.py:16`).
pub const TOTAL_TIMEOUT: Duration = Duration::from_millis(2_000);
/// A name is a handful of words (`naming_model.py:17`).
pub const MAX_TOKENS: u32 = 24;

/// The non-retrying provider a utility completion runs on, picked the way
/// `select_utility_model` picks it (`vibe/core/llm/utility_completion.py`):
/// the fast Mistral model whenever a Mistral provider is usable, the
/// session's own model otherwise. [`None`] is the reference's
/// `skip_if_no_key`: a provider whose key does not resolve is not called.
#[must_use]
pub fn utility_provider(
    routing: &ModelRouting,
    credentials: Arc<dyn Credentials>,
) -> Option<Arc<dyn CompletionProvider>> {
    let selection = utility::select(routing, credentials.as_ref()).ok()?;
    let key = &selection.provider.api_key_env_var;
    if !key.is_empty() && credentials.resolve(key).is_none() {
        return None;
    }
    let context = BackendContext::ambient(
        ApiSettings {
            timeout: REQUEST_TIMEOUT,
            retry_max_elapsed_time: Duration::ZERO,
            ..ApiSettings::default()
        },
        credentials,
    );
    let name = selection.model.name.clone();
    let completion =
        LlmCompletion::new(selection.provider, vec![selection.model], name, context).ok()?;
    Some(Arc::new(completion))
}

/// [`suggest_worktree_name`] for a caller with no runtime of its own to await
/// on, or one already inside a runtime it must not block: the request runs on
/// a thread of its own under a single-threaded runtime.
#[must_use]
pub fn suggest_worktree_name_blocking(
    prompt: Option<&str>,
    provider: Option<&dyn CompletionProvider>,
) -> Option<String> {
    let prompt = prompt.filter(|prompt| !prompt.is_empty())?;
    let provider = provider?;
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?;
                runtime.block_on(suggest_worktree_name(Some(prompt), Some(provider)))
            })
            .join()
            .ok()
            .flatten()
    })
}

/// The model's name for a worktree started by `prompt`, or [`None`].
///
/// What comes back is the model's raw text; the caller slugifies it with the
/// same rule as the prompt, so an answer that does not fit is never used
/// unfiltered.
pub async fn suggest_worktree_name(
    prompt: Option<&str>,
    provider: Option<&dyn CompletionProvider>,
) -> Option<String> {
    let prompt = prompt.filter(|prompt| !prompt.is_empty())?;
    let provider = provider?;
    match tokio::time::timeout(TOTAL_TIMEOUT, complete(prompt, provider)).await {
        Ok(answer) => answer,
        Err(_) => {
            observability::log(LogLevel::Debug, "Worktree name suggestion timed out");
            None
        }
    }
}

async fn complete(prompt: &str, provider: &dyn CompletionProvider) -> Option<String> {
    let input = ProviderInput {
        turn_id: None,
        session_id: None,
        model_override: None,
        messages: vec![
            ModelMessage::System {
                content: UtilityPrompt::WorktreeName.text().to_owned(),
            },
            ModelMessage::user(prompt.to_owned()),
        ],
        stream: false,
        images: Vec::new(),
        tools: Vec::new(),
        tool_choice: None,
        thinking: false,
        reasoning_effort: None,
        headers: std::collections::BTreeMap::new(),
        limits: RequestLimits {
            max_tokens: Some(MAX_TOKENS),
            temperature_millis: Some(0),
        },
        metadata: std::collections::BTreeMap::from([(
            "call_type".to_owned(),
            "secondary_call".to_owned(),
        )]),
    };
    match tokio::time::timeout(REQUEST_TIMEOUT, provider.complete(&input)).await {
        Ok(Ok(answer)) => {
            let text = answer.text.trim().to_owned();
            (!text.is_empty()).then_some(text)
        }
        Ok(Err(error)) => {
            observability::log(
                LogLevel::Warning,
                &format!("Worktree name suggestion failed: {error}"),
            );
            None
        }
        Err(_) => {
            observability::log(LogLevel::Debug, "Worktree name suggestion timed out");
            None
        }
    }
}

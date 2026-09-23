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

use secrecy::SecretString;

use crate::engine::CompletionProvider;
use crate::events::ModelMessage;
use crate::observability::{self, LogLevel};
use crate::prompt::library::UtilityPrompt;
use crate::provider::{
    HttpTransport, ProviderBackend, ProviderInput, ProviderStyle, RequestLimits, RetryPolicy,
};

/// One attempt's deadline (`naming_model.py:15`).
pub const REQUEST_TIMEOUT: Duration = Duration::from_millis(1_500);
/// Everything around the attempt included (`naming_model.py:16`).
pub const TOTAL_TIMEOUT: Duration = Duration::from_millis(2_000);
/// A name is a handful of words (`naming_model.py:17`).
pub const MAX_TOKENS: u32 = 24;

/// The cheap fast model background niceties prefer whenever a Mistral key
/// resolves (`vibe/core/llm/utility_completion.py:12-19`).
pub const FAST_UTILITY_MODEL: &str = "mistral-vibe-cli-fast";

/// A provider for one utility completion: the model and the endpoint, with a
/// resolved credential. [`None`] from [`utility_provider`] is the reference's
/// `skip_if_no_key`: a provider whose key does not resolve falls back at once.
pub struct UtilityModel {
    pub style: String,
    pub endpoint: String,
    pub model: String,
    pub credential: String,
}

/// The Mistral endpoint the fast utility model is reached at when the session
/// runs on another provider.
pub const MISTRAL_CHAT_COMPLETIONS: &str = "https://api.mistral.ai/v1/chat/completions";

impl UtilityModel {
    /// The model a utility completion runs on: the fast one whenever a Mistral
    /// key resolves, else the session's own `active` model
    /// (`vibe/core/llm/utility_completion.py:22-76`).
    #[must_use]
    pub fn select(active: Self, mistral_credential: Option<String>) -> Self {
        match mistral_credential.filter(|credential| !credential.is_empty()) {
            Some(credential) => Self {
                endpoint: if active.style == "mistral" {
                    active.endpoint
                } else {
                    MISTRAL_CHAT_COMPLETIONS.to_owned()
                },
                style: "mistral".to_owned(),
                model: FAST_UTILITY_MODEL.to_owned(),
                credential,
            },
            None => active,
        }
    }
}

/// Builds the non-retrying provider a utility completion runs on.
#[must_use]
pub fn utility_provider(model: UtilityModel) -> Option<Arc<dyn CompletionProvider>> {
    if model.credential.is_empty() {
        return None;
    }
    let style = ProviderStyle::parse(&model.style).ok()?;
    let transport = HttpTransport::new().ok()?;
    let backend = ProviderBackend::new(
        style,
        model.endpoint,
        model.model,
        SecretString::from(model.credential),
        transport,
    )
    .with_retry_policy(RetryPolicy {
        max_elapsed: Duration::ZERO,
        initial_delay: Duration::ZERO,
    });
    Some(Arc::new(backend))
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
            max_tokens: MAX_TOKENS,
            temperature_millis: Some(0),
            ..RequestLimits::default()
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

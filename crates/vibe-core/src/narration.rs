//! The spoken summary of a finished turn.
//!
//! Reference `NarrationService` (`vibe/app_server/_narration.py`): the fast
//! model, on the provider named `mistral` whatever the session runs on,
//! summarizes what was asked, what was answered and what failed. No such
//! provider, a key it names that does not resolve, or a call that fails
//! answers `None`, which leaves the turn silent; a call that answers with no
//! content answers an empty summary.

use serde_json::{Map, Value};

use crate::llm::retry::NoRetryObserver;
use crate::llm::types::Message;
use crate::llm::{Backend, BackendContext, ModelRequest, call, utility};
use crate::observability::{self, LogLevel};
use crate::prompt::library::UtilityPrompt;
use crate::provider::config::{BackendKind, ProviderConfig};

/// The provider the summary runs on, by name.
pub const NARRATION_PROVIDER: &str = "mistral";
/// Reference `max_tokens=512`.
pub const MAX_TOKENS: u64 = 512;

/// What one summary is asked about.
#[derive(Debug, Clone, Copy)]
pub struct TurnSummaryInput<'a> {
    pub user_message: &'a str,
    pub assistant_text: &'a str,
    pub error: Option<&'a str>,
}

/// The user message the model reads: one titled section per part the turn
/// produced, the request always.
#[must_use]
pub fn user_content(input: &TurnSummaryInput<'_>) -> String {
    let mut sections = vec![format!("## User Request\n{}", input.user_message)];
    if !input.assistant_text.is_empty() {
        sections.push(format!("## Assistant Response\n{}", input.assistant_text));
    }
    if let Some(error) = input.error.filter(|error| !error.is_empty()) {
        sections.push(format!("## Error\n{error}"));
    }
    sections.join("\n\n")
}

/// Reference `NarrationService.summarize`. `providers` are the configured
/// ones, `context` carries the configured API settings and the credentials,
/// and `metadata` the `secondary_call` request metadata.
pub async fn summarize(
    providers: &[ProviderConfig],
    context: &BackendContext,
    input: &TurnSummaryInput<'_>,
    metadata: &Map<String, Value>,
) -> Option<String> {
    let provider = providers
        .iter()
        .find(|provider| provider.name == NARRATION_PROVIDER)?
        .clone();
    if !provider.api_key_env_var.is_empty()
        && context
            .credentials
            .resolve(&provider.api_key_env_var)
            .is_none()
    {
        return None;
    }
    let backend = match Backend::new(provider, context.clone()) {
        Ok(backend) => backend,
        Err(error) => {
            observability::log(
                LogLevel::Warning,
                &format!("Turn summary generation failed: {error}"),
            );
            return None;
        }
    };
    let messages = [
        Message::system(UtilityPrompt::TurnSummary.text()),
        Message::user(user_content(input)),
    ];
    // Reference `get_user_agent(Backend.MISTRAL)`, whatever backend the
    // provider entry names.
    let headers = [(
        "user-agent".to_owned(),
        call::user_agent(BackendKind::Mistral),
    )];
    let model = utility::fast_model();
    let answer = backend
        .complete(
            &ModelRequest {
                model: &model,
                messages: &messages,
                temperature: 0.0,
                tools: None,
                max_tokens: Some(MAX_TOKENS),
                tool_choice: None,
                extra_headers: &headers,
                metadata: Some(metadata),
            },
            &NoRetryObserver,
        )
        .await;
    match answer {
        Ok(chunk) => Some(chunk.message.content.unwrap_or_default()),
        Err(error) => {
            observability::log(
                LogLevel::Warning,
                &format!("Turn summary generation failed: {error}"),
            );
            None
        }
    }
}

#[cfg(test)]
mod narration_tests;

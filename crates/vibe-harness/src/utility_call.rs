//! A call to a utility model beside a session's turns: the smart approve
//! classifier (`crate::smart`) and the background title (`crate::title`).
//!
//! Reference `vibe/_smart_approve.py` and `vibe/_title.py` in the harness
//! runtime ask their model the same way: pinned to temperature 0 with thinking
//! off, one system prompt and one user message, the session's request
//! metadata stamped with the call's type and the message it answers.

use std::collections::BTreeMap;

use vibe_core::events::ModelMessage;
use vibe_core::provider::config::{ModelConfig, ThinkingLevel};
use vibe_core::provider::{ProviderInput, RequestDialect, RequestLimits};

/// What one utility call asks, and of which model.
pub(crate) struct UtilityCall<'a> {
    pub(crate) model: &'a ModelConfig,
    /// The metadata every request of the session carries.
    pub(crate) metadata: &'a BTreeMap<String, String>,
    pub(crate) session_id: &'a str,
    /// The `call_type` the request reports.
    pub(crate) call_type: &'a str,
    /// The message the call answers, when it answers one.
    pub(crate) message_id: Option<&'a str>,
    pub(crate) system_prompt: &'a str,
}

impl UtilityCall<'_> {
    /// The request asking `prompt`, unstreamed and with the provider's own
    /// limits.
    pub(crate) fn input(&self, prompt: String) -> ProviderInput {
        let mut model = self.model.clone();
        model.temperature = 0.0;
        model.thinking = ThinkingLevel::Off;
        let mut metadata = self.metadata.clone();
        metadata.insert("call_type".to_owned(), self.call_type.to_owned());
        if let Some(message_id) = self.message_id {
            metadata.insert("message_id".to_owned(), message_id.to_owned());
        }
        ProviderInput {
            turn_id: None,
            session_id: Some(self.session_id.to_owned()),
            model_override: Some(model.alias.clone()),
            model: Some(model),
            messages: vec![
                ModelMessage::System {
                    content: self.system_prompt.to_owned(),
                },
                ModelMessage::user(prompt),
            ],
            stream: false,
            images: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            thinking: false,
            reasoning_effort: None,
            headers: BTreeMap::new(),
            limits: RequestLimits::default(),
            metadata,
            dialect: RequestDialect::Unified,
        }
    }
}

//! The parameter and settings shapes the boundary deserializes.
//!
//! Every one of these denies unknown fields: a client that sends a key this
//! version does not know is told so rather than having it silently dropped. The
//! conversions between them live here too, so a method that accepts two spellings
//! of the same write resolves them once.

use super::*;

/// Everything a `session/start` decides before it registers anything.
///
/// The precedence between what the client asked for and what the attached
/// transcript recorded is resolved once, into these values, so the registration
/// that follows reads a single answer per field instead of re-deriving it.
pub(crate) struct SessionOpening {
    pub(crate) session_id: String,
    pub(crate) working_directory: String,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) aliases: BTreeSet<String>,
    pub(crate) snapshot: Option<ProjectionSnapshot>,
    pub(crate) persisted: Option<HydratedSession>,
    pub(crate) agent_profile: AgentProfile,
    /// Whether the agent this session runs under has to be written back, which
    /// a fresh session and an explicit override both require.
    pub(crate) should_persist_agent: bool,
    pub(crate) intent: SessionIntent,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct SessionStartParams {
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    #[serde(default, rename = "cwd", alias = "workingDirectory")]
    pub(crate) working_directory: Option<String>,
    #[serde(default, rename = "workspaceRoots", alias = "addDirectories")]
    pub(crate) add_directories: Vec<String>,
    #[serde(default, rename = "trustWorkspace", alias = "trusted")]
    pub(crate) trusted: bool,
    #[serde(default)]
    pub(crate) agent: Option<String>,
    #[serde(default)]
    pub(crate) tool_filters: Vec<String>,
    #[serde(default)]
    pub(crate) enabled_tools: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) disabled_tools: Vec<String>,
    #[serde(default)]
    pub(crate) mcp_servers: Vec<Value>,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) max_turns: Option<u32>,
    #[serde(default, rename = "maxSessionTokens", alias = "maxTokens")]
    pub(crate) max_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) max_price_micros: Option<u64>,
    #[serde(default)]
    pub(crate) max_price: Option<f64>,
    #[serde(default)]
    pub(crate) mode: Option<String>,
    #[serde(default)]
    pub(crate) thinking: bool,
    #[serde(default)]
    pub(crate) reasoning_effort: Option<String>,
    #[serde(default)]
    pub(crate) auto_approve: bool,
    #[serde(default)]
    pub(crate) resume: Option<String>,
    #[serde(default, rename = "continue")]
    pub(crate) continue_session: bool,
    /// Accepted for wire compatibility. The server behaves the same either
    /// way, so nothing reads it yet.
    #[expect(dead_code, reason = "accepted for wire compatibility, not read yet")]
    #[serde(default)]
    pub(crate) headless: bool,
    #[serde(default = "default_history_limit")]
    pub(crate) history_limit: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIntent {
    pub add_directories: Vec<String>,
    pub trusted: bool,
    pub agent: Option<String>,
    pub tool_filters: Vec<String>,
    pub enabled_tools: Vec<String>,
    pub disabled_tools: Vec<String>,
    #[serde(skip)]
    pub requested_enabled_tools: Vec<String>,
    #[serde(skip)]
    pub requested_disabled_tools: Vec<String>,
    #[serde(skip)]
    pub agent_permission_rules: Vec<PermissionRule>,
    pub mcp_servers: Vec<Value>,
    pub model: Option<String>,
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u64>,
    pub max_price_micros: Option<u64>,
    pub mode: Option<String>,
    pub thinking: bool,
    pub reasoning_effort: Option<String>,
    pub auto_approve: bool,
    #[serde(skip)]
    pub requested_auto_approve: bool,
    #[serde(skip)]
    pub approval: AgentApproval,
    #[serde(default)]
    pub system_prompt_id: Option<String>,
    pub resume: Option<String>,
    #[serde(rename = "continue")]
    pub continue_session: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct SessionParams {
    pub(crate) session_id: String,
}

/// The event `telemetry/record` carries, exactly as the reference model
/// declares it: the session it belongs to, a client-authored name, free-form
/// properties and whether it correlates with the last backend request.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct TelemetryRecordParams {
    pub(crate) session_id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) properties: BTreeMap<String, Value>,
    #[serde(default)]
    pub(crate) correlate_last_request: bool,
}

/// The settings `session/settings/update` changes, which is exactly what the
/// reference declares: the two turn budgets and nothing else.
///
/// A field the reference does not declare is refused here, including the five
/// this port lets a session override. Those moved to
/// [`SessionOverridesWriteParams`] under a local method name, so a client
/// written against the reference protocol sees this method behave as its own
/// model describes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct SessionSettingsUpdateParams {
    pub(crate) session_id: String,
    #[serde(default)]
    pub(crate) max_turns: Option<u32>,
    #[serde(default)]
    pub(crate) max_tokens: Option<u64>,
}

/// What a session may override beyond the reference settings, under the local
/// method `session/overrides/write`.
///
/// Upstream none of these is session-scoped: the model and the thinking level
/// are configuration writes and the mode and the approval stance come from an
/// agent profile. This port lets a session hold them for its own lifetime,
/// which is what `vibe-cli` switches a model, a mode, a thinking level, a
/// reasoning effort and an approval stance through, and what `vibe-acp` maps
/// its session modes and config options onto. The name stays out of
/// `SERVER_METHODS` and out of the advertised capabilities, so it is offered to
/// nobody who did not already call it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct SessionOverridesWriteParams {
    pub(crate) session_id: String,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) mode: Option<String>,
    #[serde(default)]
    pub(crate) thinking: Option<bool>,
    #[serde(default)]
    pub(crate) reasoning_effort: Option<String>,
    #[serde(default)]
    pub(crate) auto_approve: Option<bool>,
}

/// The settings both methods write, whichever name carried them.
///
/// Splitting the wire shapes is what keeps the reference method exact; the
/// write itself is one operation on one session, so it stays one function.
#[derive(Debug, Default)]
pub(crate) struct SessionSettings {
    pub(crate) session_id: String,
    pub(crate) model: Option<String>,
    pub(crate) max_turns: Option<u32>,
    pub(crate) max_tokens: Option<u64>,
    pub(crate) mode: Option<String>,
    pub(crate) thinking: Option<bool>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) auto_approve: Option<bool>,
}

impl SessionSettings {
    /// The closed value sets two of these fields accept.
    const MODES: [&'static str; 2] = ["code", "plan"];
    const REASONING_EFFORTS: [&'static str; 4] = ["low", "medium", "high", "max"];

    /// Refuses a write that names nothing, or that names a value outside what
    /// the field accepts.
    pub(crate) fn validate(&self) -> Result<(), ProtocolFault> {
        if self.entries().is_empty() {
            return Err(ProtocolFault::invalid_params(
                "At least one session setting must be provided",
            ));
        }
        if self
            .mode
            .as_deref()
            .is_some_and(|mode| !Self::MODES.contains(&mode))
        {
            return Err(ProtocolFault::invalid_params(
                "mode must be `code` or `plan`",
            ));
        }
        if self
            .reasoning_effort
            .as_deref()
            .is_some_and(|effort| !Self::REASONING_EFFORTS.contains(&effort))
        {
            return Err(ProtocolFault::invalid_params(
                "reasoningEffort must be low, medium, high, or max",
            ));
        }
        Ok(())
    }

    /// What this write persists, under the keys the saved session declares.
    ///
    /// The set is also what decides whether the write named anything at all, so
    /// the emptiness check and the persistence read the same list rather than
    /// two lists that can disagree about a field.
    pub(crate) fn entries(&self) -> BTreeMap<String, Value> {
        [
            (
                "active_model",
                self.model.as_ref().map(|value| json!(value)),
            ),
            ("maxTurns", self.max_turns.map(|value| json!(value))),
            ("maxTokens", self.max_tokens.map(|value| json!(value))),
            ("mode", self.mode.as_ref().map(|value| json!(value))),
            ("thinking", self.thinking.map(|value| json!(value))),
            (
                "reasoningEffort",
                self.reasoning_effort.as_ref().map(|value| json!(value)),
            ),
            ("autoApprove", self.auto_approve.map(|value| json!(value))),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value)))
        .collect()
    }

    /// Applies what this write names to the live session, leaving every field
    /// it did not name alone.
    pub(crate) fn apply(self, intent: &mut SessionIntent) {
        if let Some(model) = self.model {
            intent.model = Some(model);
        }
        if let Some(max_turns) = self.max_turns {
            intent.max_turns = Some(max_turns);
        }
        if let Some(max_tokens) = self.max_tokens {
            intent.max_tokens = Some(max_tokens);
        }
        if let Some(mode) = self.mode {
            intent.mode = Some(mode);
        }
        if let Some(thinking) = self.thinking {
            intent.thinking = thinking;
        }
        if let Some(reasoning_effort) = self.reasoning_effort {
            intent.reasoning_effort = Some(reasoning_effort);
        }
        if let Some(auto_approve) = self.auto_approve {
            intent.auto_approve = auto_approve;
            intent.requested_auto_approve = auto_approve;
        }
    }
}

impl From<SessionSettingsUpdateParams> for SessionSettings {
    fn from(params: SessionSettingsUpdateParams) -> Self {
        Self {
            session_id: params.session_id,
            max_turns: params.max_turns,
            max_tokens: params.max_tokens,
            ..Self::default()
        }
    }
}

impl From<SessionOverridesWriteParams> for SessionSettings {
    fn from(params: SessionOverridesWriteParams) -> Self {
        Self {
            session_id: params.session_id,
            model: params.model,
            mode: params.mode,
            thinking: params.thinking,
            reasoning_effort: params.reasoning_effort,
            auto_approve: params.auto_approve,
            ..Self::default()
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct SessionCompactParams {
    pub(crate) session_id: String,
    #[serde(default)]
    pub(crate) extra_instructions: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct TurnStartParams {
    pub(crate) session_id: String,
    pub(crate) input: Vec<PublicContentBlock>,
    #[serde(default)]
    pub(crate) client_user_message_id: Option<String>,
    #[serde(default)]
    pub(crate) auto_title: Option<String>,
    #[serde(default)]
    pub(crate) user_display_content: Option<Value>,
    #[serde(default)]
    pub(crate) mention_stats: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct TurnParams {
    pub(crate) session_id: String,
    pub(crate) expected_turn_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct TurnSteerParams {
    pub(crate) session_id: String,
    pub(crate) expected_turn_id: String,
    pub(crate) input: Vec<PublicContentBlock>,
    /// Accepted for wire compatibility. Steering does not create a history
    /// entry, so neither of these two reaches the engine yet.
    #[expect(dead_code, reason = "accepted for wire compatibility, not read yet")]
    #[serde(default)]
    pub(crate) client_user_message_id: Option<String>,
    #[serde(default = "default_true")]
    pub(crate) inject_invoked_skill: bool,
    #[expect(dead_code, reason = "accepted for wire compatibility, not read yet")]
    #[serde(default)]
    pub(crate) mention_stats: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct ContextInjectParams {
    pub(crate) session_id: String,
    pub(crate) input: Vec<PublicContentBlock>,
    #[serde(default)]
    pub(crate) as_message: bool,
    /// Accepted for wire compatibility; injection does not resolve mentions
    /// yet.
    #[serde(default)]
    pub(crate) inject_invoked_skill: bool,
    #[serde(default)]
    pub(crate) client_user_message_id: Option<String>,
    #[expect(dead_code, reason = "accepted for wire compatibility, not read yet")]
    #[serde(default)]
    pub(crate) mention_stats: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct CallbackResponseParams {
    pub(crate) session_id: String,
    pub(crate) callback_id: String,
    pub(crate) output: Value,
}

pub(crate) fn content_text(input: &[PublicContentBlock]) -> String {
    input
        .iter()
        .filter_map(|block| match block {
            PublicContentBlock::Text { text } => Some(text.as_str()),
            PublicContentBlock::Image { .. } | PublicContentBlock::Resource { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(crate) const fn default_true() -> bool {
    true
}

pub(crate) fn price_dollars_to_micros(price: f64) -> Option<u64> {
    (price.is_finite() && price >= 0.0 && price <= u64::MAX as f64 / 1_000_000.0)
        .then(|| (price * 1_000_000.0).round() as u64)
}

pub(crate) fn review_message_index(
    release3: &Release3Service,
    session: &SessionRuntime,
) -> Result<usize, ServerError> {
    release3
        .message_count(&session.id)
        .map_err(|error| ServerError::Resource(error.to_string()))
        .map(|message_count| {
            message_count.unwrap_or_else(|| {
                session
                    .persisted
                    .as_ref()
                    .map(|persisted| persisted.messages.len())
                    .unwrap_or_default()
            })
        })
}

pub(crate) const fn default_history_limit() -> u16 {
    200
}

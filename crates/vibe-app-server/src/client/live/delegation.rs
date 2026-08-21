//! What a live turn delegates: a subagent it forks a child session for, and an
//! MCP sampling request it answers from its own provider.
//!
//! Both reach the same backend the turn runs on, which is why they live beside
//! the driver rather than in the core: the core states what a subagent and a
//! sampling request are, and this binds them to a provider.

use super::*;

/// Reference `TaskArgs.agent` default.
pub(crate) const DEFAULT_SUBAGENT: &str = "explore";

/// Directive coverage for `task`, whose reference description this port must
/// cover without reproducing (`NOTICE`).
///
/// | Reference directive | Covered by |
/// |---|---|
/// | The work is handed to a specialized subagent | "Hand a bounded task to a subagent" |
/// | The subagent runs in its own session and reports back once | "runs in its own session and reports back once" |
/// | The task text is self-contained, because the subagent sees no history | "state it self-contained: the subagent sees none of this conversation" |
/// | The agent name selects which specialization runs | the `agent` description |
///
/// The argument shape comes from the reference `TaskArgs`, which configures
/// `extra="forbid"`: `agent` is a plain string carrying a default rather than
/// an enum of the discovered names, so a schema built here never depends on
/// what the local catalog happens to hold.
pub(crate) fn task_spec() -> ToolSpec {
    ToolSpec {
        name: "task".to_owned(),
        description: "Hand a bounded task to a subagent, which runs in its own session and \
                      reports back once. State the task self-contained: the subagent sees none \
                      of this conversation."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .required(
                "task",
                Property::string().described("The task for the subagent to perform"),
            )
            .optional(
                "agent",
                Property::string()
                    .described("Which specialized subagent runs the task")
                    .with_default(DEFAULT_SUBAGENT),
            )
            .forbid_extra_properties()
            .build(),
        output_schema: None,
        config: Value::Null,
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: vibe_core::tools::ToolPresentationKind::Generic,
        source: ToolSource::BuiltIn,
        selection_priority: 40,
    }
}

pub(super) struct ProviderSubagentRunner {
    provider: Arc<dyn CompletionProvider>,
    system_prompt: String,
    store: SessionStore,
    tools: ToolRegistry,
    input_price_per_million_micros: u64,
    output_price_per_million_micros: u64,
    parent_intent: SessionIntent,
}

/// Answers an MCP sampling request with the provider this driver already runs
/// turns on.
///
/// Reference `MCPSamplingHandler` returns a structured error rather than a
/// partial completion when the backend fails, and names the model it answered
/// with, so a server can tell which one produced the text it received.
pub(super) struct ProviderSamplingHandler {
    pub(super) provider: Arc<dyn CompletionProvider>,
    pub(super) model: String,
}

impl LiveTurnDriver {
    pub(super) fn register_task_tool(
        &self,
        reservation: &TurnReservation,
        store: SessionStore,
        parent_session_id: String,
    ) -> Result<(), DriverError> {
        let built_in = built_in_subagent();
        let vibe_home = crate::host::vibe_home();
        let catalog = discover_extensions(
            &DiscoveryRoots {
                configured: Vec::new(),
                project: vec![PathBuf::from(&reservation.working_directory).join(".vibe")],
                user: vec![vibe_home.join("extensions")],
                project_trusted: reservation.intent.trusted,
                // Only the agent profiles are read here, so no skill root is
                // resolved and no skill is walked.
                ..DiscoveryRoots::default()
            },
            BTreeMap::from([(built_in.name.clone(), built_in)]),
            BTreeMap::new(),
            BTreeMap::new(),
        );
        let subagents = catalog
            .agents
            .into_iter()
            .filter(|(_, profile)| profile.kind == AgentKind::Subagent)
            .collect::<BTreeMap<_, _>>();
        let runner = Arc::new(ProviderSubagentRunner {
            provider: self.provider.clone(),
            system_prompt: self.system_prompt.clone(),
            store: store.clone(),
            tools: reservation.tools.clone(),
            input_price_per_million_micros: self.input_price_per_million_micros,
            output_price_per_million_micros: self.output_price_per_million_micros,
            parent_intent: reservation.intent.clone(),
        });
        let manager = Arc::new(SubagentManager::new(store, runner));
        // `task` is published behind the same composition as every other
        // builtin. The guard travels on the registry because this registration
        // happens a layer above the one that built it, and a session whose
        // builtin surface never registered has no policy to delegate under, so
        // the tool is withheld rather than published unguarded.
        let guard = reservation.tools.guard().ok_or_else(|| {
            DriverError::Tool(
                "`task` cannot be published for a session with no permission guard".to_owned(),
            )
        })?;
        let settings = guard.config.clone();
        reservation
            .tools
            .register(
                task_spec(),
                Arc::new(PolicyGuardedTool::new(
                    "task",
                    guard.policy.clone(),
                    guard.approval.clone(),
                    Arc::new(move |invocation: &vibe_core::tools::ToolInvocation| {
                        Ok(resolve_task_tool_permission(
                            requested_agent(&invocation.arguments),
                            &settings.view::<SharedToolConfig>("task"),
                        ))
                    }),
                    task_handler(manager, subagents, parent_session_id),
                )),
            )
            .map(drop)
            .map_err(|error| DriverError::Tool(error.to_string()))
    }
}

/// Which subagent a call names, with the reference `TaskArgs.agent` default
/// applied. The policy resolver and the handler read the argument through this
/// one function, so an absent key cannot mean `explore` to one and nothing to
/// the other.
fn requested_agent(arguments: &Value) -> &str {
    arguments
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_SUBAGENT)
}

/// The subagent every session publishes before any extension is discovered.
///
/// The delegation oracle seeds its own catalog from this, so the name the
/// `agent` argument resolves to there is the one production offers rather than
/// a second spelling of it.
pub(crate) fn built_in_subagent() -> AgentProfile {
    AgentProfile {
        name: DEFAULT_SUBAGENT.to_owned(),
        display_name: "Explore".to_owned(),
        description: "Inspect a bounded task in an independent child session".to_owned(),
        kind: AgentKind::Subagent,
        safety: "read_only".to_owned(),
        overrides: toml::Table::new(),
        source: ExtensionSource::Builtin,
        path: None,
    }
}

/// The `task` handler, over the delegation manager and the subagent catalog the
/// `agent` argument is resolved against.
///
/// It is a free function rather than a closure built inside the registration
/// because the delegation oracle
/// (`crates/vibe-app-server/src/tool_execution_parity_tests.rs`) drives this
/// exact handler with a scripted runner: a handler reachable only through a
/// live provider could not be measured against the reference.
pub(crate) fn task_handler(
    manager: Arc<SubagentManager>,
    subagents: BTreeMap<String, AgentProfile>,
    parent_session_id: String,
) -> Arc<dyn ToolHandler> {
    let subagent_names = Arc::new(subagents.keys().cloned().collect::<Vec<_>>());
    Arc::new(
        move |invocation: &vibe_core::tools::ToolInvocation,
              _output: vibe_core::tools::ToolOutputSink|
              -> OwnedToolHandlerFuture {
            let manager = manager.clone();
            let subagents = subagents.clone();
            let subagent_names = subagent_names.clone();
            let parent_session_id = parent_session_id.clone();
            let arguments = invocation.arguments.clone();
            Box::pin(async move {
                let agent_name = requested_agent(&arguments);
                let task = arguments
                    .get("task")
                    .and_then(Value::as_str)
                    .filter(|task| !task.is_empty())
                    .ok_or_else(|| vibe_core::tools::ToolError::SchemaViolation {
                        path: "/task".to_owned(),
                        message: "must be a non-empty string".to_owned(),
                    })?;
                let agent = subagents.get(agent_name).cloned().ok_or_else(|| {
                    // A model that guessed the name corrects itself from
                    // the list rather than from a bare refusal.
                    vibe_core::tools::ToolError::Unavailable(format!(
                        "subagent `{agent_name}` is unavailable; available agents: {}",
                        if subagent_names.is_empty() {
                            "none".to_owned()
                        } else {
                            subagent_names.join(", ")
                        }
                    ))
                })?;
                let effect = manager
                    .delegate(
                        DelegationRequest {
                            parent_session_id,
                            agent,
                            prompt: task.to_owned(),
                            logging: ChildLoggingPolicy::SummaryOnly,
                        },
                        crate::host::now_millis(),
                    )
                    .await
                    .map_err(|error| vibe_core::tools::ToolError::Execution(error.to_string()))?;
                // Reference `TaskResult` (`vibe/core/subagents.py:26`) declares
                // `response`, `turns_used` and `completed` and nothing else, so
                // the delegation effect stays in the display payload the client
                // reads and only those three reach the model.
                let typed_result = json!({
                    "response": effect.result,
                    "turns_used": effect.turns_used,
                    "completed": effect.completed,
                });
                let model_text = reference_text::joined(&[
                    ("response", effect.result.clone()),
                    ("turns_used", effect.turns_used.to_string()),
                    (
                        "completed",
                        reference_text::boolean(effect.completed).to_owned(),
                    ),
                ]);
                Ok(ToolExecutionOutput {
                    model_text,
                    typed_result,
                    display: json!({"kind": "subagent", "effect": effect}),
                    projected_result: serde_json::Value::Null,
                    chunks: Vec::new(),
                })
            })
        },
    )
}

impl SubagentRunner for ProviderSubagentRunner {
    fn run<'a>(
        &'a self,
        context: ChildContext,
        cancellation: CancellationToken,
    ) -> SubagentFuture<'a> {
        Box::pin(async move {
            let metadata = self
                .store
                .load(&context.child_session_id)
                .map_err(|error| error.to_string())?
                .metadata;
            let parent_executor = SessionToolExecutor::new(self.tools.clone(), &self.parent_intent);
            let settings = context.agent.runtime_settings();
            // An agent declares its two lists in the same form the session does,
            // so they are matched by the same reference rules rather than by
            // exact name.
            let enabled_by_agent = context
                .agent
                .overrides
                .contains_key("enabled_tools")
                .then(|| NameFilter::new(&settings.enabled_tools));
            let disabled_by_agent = NameFilter::new(&settings.disabled_tools);
            let policy_restricted_tools = settings
                .permission_rules
                .iter()
                .map(|rule| rule.tool.clone())
                .collect::<BTreeSet<_>>();
            let allowed = self
                .tools
                .available(&NameFilter::default(), &NameFilter::default())
                .map_err(|error| error.to_string())?
                .into_iter()
                .filter(|spec| {
                    parent_executor.permits(&spec.name)
                        && spec.name != "task"
                        && !disabled_by_agent.matches(&spec.name)
                        && !policy_restricted_tools.contains(&spec.name)
                        && enabled_by_agent
                            .as_ref()
                            .is_none_or(|enabled| enabled.matches(&spec.name))
                        && (context.agent.safety != "read_only"
                            || matches!(
                                spec.presentation,
                                ToolPresentationKind::Read | ToolPresentationKind::Search
                            ))
                })
                .map(|spec| spec.name)
                .collect();
            let executor = parent_executor.with_allowed_tools(allowed);
            let definitions = executor.definitions().map_err(|error| error.to_string())?;
            let mut messages = vec![ModelMessage::System {
                content: self.system_prompt.clone(),
            }];
            if let Some(prompt_id) = settings.system_prompt_id.as_deref() {
                let prompt = crate::builtin_agents::system_prompt(prompt_id).ok_or_else(|| {
                    format!(
                        "agent `{}` references unsupported system prompt `{prompt_id}`",
                        context.agent.name
                    )
                })?;
                messages.push(ModelMessage::System {
                    content: prompt.to_owned(),
                });
            }
            let input = ProviderInput {
                turn_id: Some(format!("{}-turn", context.child_session_id)),
                model_override: settings.model,
                messages,
                stream: true,
                images: Vec::new(),
                tools: definitions,
                tool_choice: None,
                thinking: settings.thinking.unwrap_or(false),
                reasoning_effort: settings.reasoning_effort,
                headers: BTreeMap::new(),
                limits: RequestLimits {
                    max_tokens: 4096,
                    temperature_millis: None,
                    max_response_bytes: 2 * 1024 * 1024,
                },
                metadata: BTreeMap::from([
                    ("parent_session_id".to_owned(), context.parent_session_id),
                    ("agent".to_owned(), context.agent.name),
                    ("working_directory".to_owned(), context.working_directory),
                ]),
            };
            let outcome = ConversationEngine::new(self.provider.clone())
                .with_tools(executor)
                .with_sink(SessionTranscriptSink::new(self.store.clone(), metadata))
                .with_limits(EngineLimits {
                    input_price_per_million_micros: self.input_price_per_million_micros,
                    output_price_per_million_micros: self.output_price_per_million_micros,
                    ..EngineLimits::default()
                })
                .run_turn(
                    context.child_session_id,
                    input,
                    context.prompt,
                    cancellation,
                )
                .await
                .map_err(|error| error.to_string())?;
            // Reference `_sessions.py:322` counts one turn per assistant message
            // in the child transcript and calls the run complete only when the
            // child turn reached its own end, which is what the stop reason
            // reports here.
            let turns_used = outcome
                .messages
                .iter()
                .filter(|message| matches!(message, ModelMessage::Assistant { .. }))
                .count();
            Ok(SubagentRun {
                response: outcome
                    .messages
                    .iter()
                    .rev()
                    .find_map(|message| match message {
                        ModelMessage::Assistant { content, .. } if !content.is_empty() => {
                            Some(content.clone())
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| "Subagent completed without a text response".to_owned()),
                turns_used: u32::try_from(turns_used).unwrap_or(u32::MAX),
                completed: outcome.stop_reason == TurnStopReason::Complete,
            })
        })
    }
}

impl SamplingHandler for ProviderSamplingHandler {
    fn complete<'a>(&'a self, request: SamplingRequest) -> McpFuture<'a, SamplingResponse> {
        Box::pin(async move {
            let input = ProviderInput {
                turn_id: None,
                model_override: None,
                messages: request
                    .messages
                    .into_iter()
                    .map(|message| match message.role {
                        SamplingRole::System => ModelMessage::System {
                            content: message.content,
                        },
                        SamplingRole::User => ModelMessage::user(message.content),
                        SamplingRole::Assistant => ModelMessage::Assistant {
                            content: message.content,
                            reasoning: None,
                            reasoning_signature: None,
                            reasoning_state: Vec::new(),
                            tool_calls: Vec::new(),
                        },
                    })
                    .collect(),
                stream: false,
                images: Vec::new(),
                tools: Vec::new(),
                tool_choice: None,
                thinking: false,
                reasoning_effort: None,
                headers: BTreeMap::new(),
                limits: RequestLimits {
                    max_tokens: request.max_tokens.unwrap_or(4096),
                    temperature_millis: request.temperature_millis,
                    max_response_bytes: 2 * 1024 * 1024,
                },
                metadata: BTreeMap::from([("operation".to_owned(), "mcp_sampling".to_owned())]),
            };
            let message = self
                .provider
                .complete(&input)
                .await
                .map_err(|error| McpError::Tool(error.to_string()))?;
            Ok(SamplingResponse {
                text: message.text,
                model: self.model.clone(),
            })
        })
    }
}

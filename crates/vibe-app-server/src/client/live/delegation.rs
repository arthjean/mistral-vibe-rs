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
    /// Composes the child's own system message, when the server composed the
    /// parent's; `system_prompt` stands in for it otherwise.
    subagent_prompt: Option<crate::client::SubagentPromptComposer>,
    /// Resolves the model the child's own configuration runs, when the server
    /// composed the parent's prompt; the parent's model stands in otherwise.
    subagent_model: Option<crate::client::SubagentModelResolver>,
    tools: ToolRegistry,
    input_price_per_million_micros: u64,
    output_price_per_million_micros: u64,
    /// The prices the child's saved accounting records, the parent's own.
    pricing: (f64, f64, Option<f64>),
    parent_intent: SessionIntent,
    /// The parent's hooks, which a child loads as its own (reference
    /// `hook_config_result` handed to the child loop).
    hooks: crate::session_hooks::SessionHooks,
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
        // Reference `Task.run` resolves the name through the session's own
        // `AgentManager`, so what `task` may start is what the session is
        // offered: primary agents included, which the handler then refuses.
        let agents = reservation
            .system_prompt
            .as_ref()
            .map_or_else(|| vec![built_in_subagent()], |prompt| prompt.agents.clone())
            .into_iter()
            .map(|profile| (profile.name.clone(), profile))
            .collect::<BTreeMap<_, _>>();
        let runner = Arc::new(ProviderSubagentRunner {
            provider: self.provider.clone(),
            system_prompt: self.system_prompt.clone(),
            subagent_prompt: reservation
                .system_prompt
                .as_ref()
                .map(|prompt| Arc::clone(&prompt.subagent)),
            subagent_model: reservation
                .system_prompt
                .as_ref()
                .map(|prompt| Arc::clone(&prompt.subagent_model)),
            tools: reservation.tools.clone(),
            input_price_per_million_micros: self.input_price_per_million_micros,
            output_price_per_million_micros: self.output_price_per_million_micros,
            pricing: reservation.pricing,
            parent_intent: reservation.intent.clone(),
            hooks: reservation.hooks.clone(),
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
                    task_handler(manager, agents, parent_session_id),
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

/// The `task` handler, over the delegation manager and the agents the `agent`
/// argument is resolved against.
///
/// It is a free function rather than a closure built inside the registration
/// because the delegation oracle
/// (`crates/vibe-app-server/src/tool_execution_parity_tests.rs`) drives this
/// exact handler with a scripted runner: a handler reachable only through a
/// live provider could not be measured against the reference.
pub(crate) fn task_handler(
    manager: Arc<SubagentManager>,
    agents: BTreeMap<String, AgentProfile>,
    parent_session_id: String,
) -> Arc<dyn ToolHandler> {
    Arc::new(
        move |invocation: &vibe_core::tools::ToolInvocation,
              output: vibe_core::tools::ToolOutputSink|
              -> OwnedToolHandlerFuture {
            let manager = manager.clone();
            let agents = agents.clone();
            let parent_session_id = parent_session_id.clone();
            let arguments = invocation.arguments.clone();
            let tool_call_id = invocation.call_id.clone();
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
                // Reference `Task.run`: a name the session is not offered is
                // unknown, and a primary agent is refused, so a delegated run
                // never starts a second top-level loop.
                let agent = agents.get(agent_name).cloned().ok_or_else(|| {
                    vibe_core::tools::ToolError::Unavailable(format!(
                        "no agent named `{agent_name}` can be delegated to"
                    ))
                })?;
                if agent.kind != AgentKind::Subagent {
                    return Err(vibe_core::tools::ToolError::Unavailable(format!(
                        "`{agent_name}` is a primary agent, and `task` only starts subagents: \
                         a delegated run may not open another top-level loop"
                    )));
                }
                // The child's identifier reaches the parent's effect as soon as
                // the child is saved, and each call the child settles reaches it
                // as a progress line, as reference `link_subagent` and the
                // `ToolStreamEvent`s of `SubagentRunAccumulator` do.
                let signal: DelegationSignal = Arc::new(move |update| {
                    // A projection that is gone has no one left to inform, so
                    // a failed forward is dropped rather than failing the run.
                    let _ = match update {
                        DelegationUpdate::Linked(child) => output.link_child_session(&child),
                        DelegationUpdate::Progress(line) => output.emit(line),
                    };
                });
                let effect = manager
                    .delegate(
                        DelegationRequest {
                            parent_session_id,
                            tool_call_id,
                            agent,
                            prompt: task.to_owned(),
                            logging: ChildLoggingPolicy::SummaryOnly,
                            signal: Some(signal),
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
                    skip: None,
                    turn_failure: None,
                    approval: None,
                    failure: None,
                    model_text,
                    typed_result,
                    display: json!({"kind": "subagent", "effect": effect}),
                    projected_result: serde_json::Value::Null,
                    chunks: Vec::new(),
                    pending_injection: None,
                })
            })
        },
    )
}

/// The first message a child reads: where its parent's scratchpad is, then
/// the task (reference `prepare_subagent_prompt`, in this port's own words).
fn subagent_prompt(task: &str, scratchpad: Option<&Path>) -> String {
    match scratchpad {
        Some(directory) => format!(
            "Your parent session's scratchpad is {}; files there can be read and written \
             without asking.\n\n{task}",
            directory.display()
        ),
        None => task.to_owned(),
    }
}

/// Watches a child's events for what its parent is told while it runs:
/// reference `SubagentRunAccumulator.observe`, which reports each call the
/// child settles as `{tool}: {result header}` and marks the run unfinished
/// when a call was skipped.
struct SubagentProgress {
    reducer: Mutex<ProjectionReducer>,
    signal: Option<DelegationSignal>,
    skipped: std::sync::atomic::AtomicBool,
}

impl EventObserver for SubagentProgress {
    fn observe(&self, envelope: &vibe_core::events::EventEnvelope) -> Result<(), String> {
        let mut reducer = self
            .reducer
            .lock()
            .map_err(|_| "subagent progress lock is poisoned".to_owned())?;
        // The engine already validated the stream; a refusal here only means
        // this watcher cannot render the header, never that the run failed.
        let _ = reducer.apply(envelope);
        let vibe_core::events::EngineEvent::ToolResult {
            call_id, skipped, ..
        } = &envelope.event
        else {
            return Ok(());
        };
        if *skipped {
            self.skipped
                .store(true, std::sync::atomic::Ordering::Release);
            return Ok(());
        }
        let line = reducer
            .state()
            .history
            .iter()
            .find_map(|entry| match entry {
                vibe_core::events::PublicHistoryEntry::Effect {
                    metadata,
                    detail,
                    state: vibe_core::events::PublicEffectState::Completed { display, .. },
                    ..
                } if metadata.id == *call_id => Some(format!(
                    "{}: {}",
                    detail.tool_name,
                    format!("{} {}", display.verb, display.message).trim()
                )),
                _ => None,
            });
        if let (Some(line), Some(signal)) = (line, &self.signal) {
            signal(DelegationUpdate::Progress(line));
        }
        Ok(())
    }
}

impl SubagentRunner for ProviderSubagentRunner {
    fn run<'a>(
        &'a self,
        context: ChildContext,
        cancellation: CancellationToken,
    ) -> SubagentFuture<'a> {
        Box::pin(async move {
            let metadata = context
                .store
                .open(&context.child_session_id)
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
                .available(None, &NameFilter::default())
                .map_err(|error| error.to_string())?
                .into_iter()
                .filter(|spec| {
                    // `task` stays in the child's surface, matching the
                    // reference, whose depth ceiling is enforced when the call
                    // runs rather than by withholding the tool.
                    parent_executor.permits(&spec.name)
                        && !disabled_by_agent.matches(&spec.name)
                        && !policy_restricted_tools.contains(&spec.name)
                        && enabled_by_agent
                            .as_ref()
                            .is_none_or(|enabled| enabled.matches(&spec.name))
                })
                .map(|spec| spec.name)
                .collect();
            let mut executor = parent_executor.with_allowed_tools(allowed);
            // The `task` this child sees is the one its parent registered, so
            // the delegation it would ask for names the parent's session and
            // passes the ceiling the parent already passed. The child reads the
            // ceiling off its own depth instead, and reads it here rather than
            // by losing the tool.
            if let Some(refusal) = context.delegation_refusal() {
                executor = executor.refusing("task", refusal);
            }
            let definitions = executor.definitions().map_err(|error| error.to_string())?;
            // Reference `_create_subagent_loop` renders the child's prompt from
            // its own profile: its `system_prompt_id`, its skills and subagents,
            // and no scratchpad.
            let content = match &self.subagent_prompt {
                Some(compose) => compose(&context.agent)?,
                None => self.system_prompt.clone(),
            };
            let messages = vec![ModelMessage::System { content }];
            let model = self
                .subagent_model
                .as_ref()
                .and_then(|resolve| resolve(&context.agent));
            let input = ProviderInput {
                turn_id: Some(format!("{}-turn", context.child_session_id)),
                session_id: None,
                model_override: settings.model,
                model,
                messages,
                stream: true,
                images: Vec::new(),
                tools: definitions,
                tool_choice: None,
                thinking: settings.thinking.unwrap_or(false),
                reasoning_effort: settings.reasoning_effort,
                headers: BTreeMap::new(),
                limits: RequestLimits::default(),
                metadata: BTreeMap::from([
                    (
                        "parent_session_id".to_owned(),
                        context.parent_session_id.clone(),
                    ),
                    ("agent".to_owned(), context.agent.name.clone()),
                    (
                        "working_directory".to_owned(),
                        context.working_directory.clone(),
                    ),
                ]),
            };
            // Reference: the child loop builds its own `HooksManager` from the
            // parent's hooks, and its invocations name the parent session.
            let transcript_path = if context.logging == ChildLoggingPolicy::Disabled {
                String::new()
            } else {
                crate::session_hooks::transcript_path(&context.store.session_path(&metadata))
            };
            let hooks = crate::session_hooks::SessionHooks::from_config(
                self.hooks.config().clone(),
                std::path::Path::new(&context.working_directory),
            )
            .turn_hooks(
                transcript_path,
                context.working_directory.clone(),
                Some(context.parent_session_id.clone()),
            );
            let progress = Arc::new(SubagentProgress {
                reducer: Mutex::new(ProjectionReducer::new(context.child_session_id.clone())),
                signal: context.signal.clone(),
                skipped: std::sync::atomic::AtomicBool::new(false),
            });
            let mut engine = ConversationEngine::new(self.provider.clone())
                .with_tools(executor)
                .with_working_directory(context.working_directory)
                .with_sink(
                    SessionTranscriptSink::new(context.store.clone(), metadata)
                        .with_pricing(self.pricing),
                )
                .with_observer(progress.clone())
                .with_limits(EngineLimits {
                    input_price_per_million_micros: self.input_price_per_million_micros,
                    output_price_per_million_micros: self.output_price_per_million_micros,
                    ..EngineLimits::default()
                });
            if let Some(hooks) = hooks {
                engine = engine.with_hooks(hooks);
            }
            // Reference `prepare_subagent_prompt` names the parent's
            // scratchpad before the task, so the child can hand files back.
            let scratchpad = vibe_core::scratchpad::init_scratchpad(&context.parent_session_id);
            let prompt = subagent_prompt(&context.prompt, scratchpad.as_deref());
            let outcome = engine
                .run_turn(
                    context.child_session_id.clone(),
                    input,
                    prompt,
                    cancellation,
                )
                .await;
            // Reference `SessionRuntimeRegistry.run` reads the child's
            // transcript whatever the turn ended with: a failed turn still
            // hands back what the child said before it failed, followed by the
            // error, and never counts as complete.
            let (messages, completed, error) = match outcome {
                Ok(outcome) => (
                    outcome.messages,
                    outcome.stop_reason == TurnStopReason::Complete,
                    None,
                ),
                Err(error) => (
                    context
                        .store
                        .open(&context.child_session_id)
                        .map(|session| session.messages)
                        .unwrap_or_default(),
                    false,
                    Some(error.to_string()),
                ),
            };
            // Reference `_sessions.py` counts one turn per assistant message in
            // the child transcript, and `SubagentRunAccumulator`
            // (`vibe/core/subagents.py:41`) joins the content of every
            // assistant message in order with no separator, so the narration
            // around the child's tool calls reaches the parent with its answer.
            let assistant = messages
                .iter()
                .filter_map(|message| match message {
                    ModelMessage::Assistant { content, .. } => Some(content.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let mut response = assistant.concat();
            if let Some(error) = &error {
                response.push_str(&format!("\n[Subagent error: {error}]"));
            }
            Ok(SubagentRun {
                response,
                turns_used: u32::try_from(assistant.len()).unwrap_or(u32::MAX),
                completed: completed
                    && !progress.skipped.load(std::sync::atomic::Ordering::Acquire),
            })
        })
    }
}

impl SamplingHandler for ProviderSamplingHandler {
    fn complete<'a>(&'a self, request: SamplingRequest) -> McpFuture<'a, SamplingResponse> {
        Box::pin(async move {
            let input = ProviderInput {
                turn_id: None,
                session_id: None,
                model_override: None,
                model: None,
                messages: request
                    .messages
                    .into_iter()
                    .map(|message| match message.role {
                        SamplingRole::System => ModelMessage::System {
                            content: message.content,
                        },
                        SamplingRole::User => ModelMessage::user(message.content),
                        SamplingRole::Assistant => ModelMessage::Assistant {
                            message_id: None,
                            reasoning_message_id: None,
                            content: message.content,
                            reasoning: None,
                            reasoning_payloads: Vec::new(),
                            tool_calls: Vec::new(),
                            keeps_empty_content: false,
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
                    max_tokens: request.max_tokens,
                    temperature_millis: request.temperature_millis,
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

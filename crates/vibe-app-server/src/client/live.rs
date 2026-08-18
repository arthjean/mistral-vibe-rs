//! The driver that runs a turn against a real completion provider.
//!
//! Everything a live turn needs is here: how the request is composed, what the
//! session's tools resolve to, how the transcript it continues is opened, and
//! how compaction, subagents and MCP sampling reach the same provider the turn
//! runs on. The module above owns the contract; this owns the one implementation
//! that talks to a backend.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use secrecy::SecretString;
use serde_json::{Value, json};

use vibe_core::compaction::CompactionFailure;
use vibe_core::compaction::manager::{
    self as compaction_manager, CompactionPlan, CompactionPromptResolution,
};
use vibe_core::engine::{
    CancellationToken, CompactionResult, Compactor, CompletionProvider, CompositeEventObserver,
    ConversationEngine, EngineLimits, EventObserver, NoopEventObserver, SessionStats,
    SessionTranscriptSink, ToolExecutor, ToolFuture, ToolStreamSink, TurnControl,
    TurnControlHandle, TurnOutcome,
};
use vibe_core::events::ModelMessage;
use vibe_core::extensions::{
    AgentKind, AgentProfile, ChildContext, ChildLoggingPolicy, DelegationRequest, DiscoveryRoots,
    ExtensionSource, SubagentFuture, SubagentManager, SubagentRunner, discover_extensions,
};
use vibe_core::matching::NameFilter;
use vibe_core::mcp::{
    McpError, McpFuture, SamplingHandler, SamplingRequest, SamplingResponse, SamplingRole,
};
use vibe_core::middleware::{CompactionSettings, ContextWarningMiddleware};
use vibe_core::provider::{
    HttpTransport, ProviderBackend, ProviderInput, ProviderStyle, RequestLimits, ToolChoice,
    ToolDefinition,
};
use vibe_core::schema::{ObjectSchema, Property};
use vibe_core::session_id::rotate_session_id;
use vibe_core::storage::SessionStore;
use vibe_core::tools::{
    OwnedToolHandlerFuture, ToolAvailability, ToolExecutionOutput, ToolHandler,
    ToolPresentationKind, ToolRegistry, ToolSource, ToolSpec,
};

pub(crate) mod delegation;

use delegation::ProviderSamplingHandler;

use super::interactive::plan_file_path;
use super::{
    CompactionDriverFuture, DriverError, DriverFuture, PublicContentBlock, SessionCompaction,
    SessionIntent, TurnDriver, TurnReservation, provider_images, session_stats,
};

#[derive(Debug, Clone)]
pub struct LiveDriverConfig {
    pub style: String,
    pub endpoint: String,
    pub model: String,
    pub credential_environment: String,
    pub system_prompt: String,
    pub session_root: Option<PathBuf>,
    pub input_price_per_million_micros: u64,
    pub output_price_per_million_micros: u64,
    /// The three compaction texts this process summarizes under, already
    /// resolved through `compaction_prompt_id`.
    pub compaction_prompts: CompactionPromptResolution,
}

/// One `context/inject` entry waiting for the next turn, carrying the wire
/// flag that decides whether a skill invocation in it appends the synthetic
/// pair when the entry becomes a message.
#[derive(Debug, Clone)]
struct PendingContext {
    content: String,
    inject_invoked_skill: bool,
}

/// The persisted half of a turn: the transcript the store already holds, the
/// identity the engine runs the turn under, and the metadata the sink and the
/// baseline are built from.
///
/// A driver with no session root has none of this, which is the only difference
/// between a persisted turn and an ephemeral one.
struct TurnTranscript {
    store: SessionStore,
    metadata: vibe_core::storage::SessionMetadata,
    session_id: String,
    messages: Vec<ModelMessage>,
}

pub struct LiveTurnDriver {
    provider: Arc<dyn CompletionProvider>,
    compactor: ProviderSessionCompactor,
    system_prompt: String,
    session_root: Option<PathBuf>,
    input_price_per_million_micros: u64,
    output_price_per_million_micros: u64,
    controls: Mutex<HashMap<(String, String), LiveTurnControl>>,
    pending_context: Mutex<HashMap<String, Vec<PendingContext>>>,
    /// The context-warning policy each session latches on.
    ///
    /// The engine is rebuilt for every turn, so a policy that speaks once per
    /// session cannot live on it. It lives here, where it outlives the turns,
    /// and the engine borrows it for the length of each one.
    context_warnings: Mutex<HashMap<String, Arc<ContextWarningMiddleware>>>,
    event_observer: Arc<dyn EventObserver>,
}

/// The provider-bound half of compaction: it mints the identifier the compacted
/// session continues under and hands everything else to the core manager.
///
/// The summarization itself lives one layer down, in
/// [`vibe_core::compaction::manager`], because it is provider-neutral: the call
/// shape, the failure taxonomy, the fallback and the retry ladder are the same
/// whichever backend answers, and keeping them there is what lets the compaction
/// corpus drive them with a scripted provider.
#[derive(Clone)]
pub(super) struct ProviderSessionCompactor {
    provider: Arc<dyn CompletionProvider>,
    /// The prompts, the model, the tools and the strict flag this session
    /// summarizes under.
    plan: Arc<CompactionPlan>,
}

impl ProviderSessionCompactor {
    pub(super) fn new(provider: Arc<dyn CompletionProvider>) -> Self {
        Self {
            provider,
            plan: Arc::new(CompactionPlan::default()),
        }
    }

    /// The same compactor, summarizing under `plan`.
    pub(super) fn with_plan(&self, plan: CompactionPlan) -> Self {
        Self {
            provider: Arc::clone(&self.provider),
            plan: Arc::new(plan),
        }
    }

    async fn compact_with_instructions(
        &self,
        current_session_id: &str,
        messages: &[ModelMessage],
        extra_instructions: &str,
    ) -> Result<CompactionResult, CompactionFailure> {
        let summarized = compaction_manager::compact(
            self.provider.as_ref(),
            &self.plan,
            messages,
            extra_instructions.trim(),
        )
        .await?;
        Ok(CompactionResult {
            new_session_id: rotate_session_id(current_session_id),
            summary: summarized.summary,
            messages: summarized.messages,
            usage: summarized.usage,
            failure: summarized.failure,
        })
    }

    /// The plan one session summarizes under: this process's resolved prompts,
    /// with the model, the strict flag and the live tool surface the session
    /// itself carries.
    fn session_plan(
        &self,
        settings: &CompactionSettings,
        tools: Vec<ToolDefinition>,
        tool_choice: Option<ToolChoice>,
        thinking: bool,
    ) -> CompactionPlan {
        CompactionPlan {
            prompts: self.plan.prompts.clone(),
            model: settings.compaction_model.clone(),
            thinking,
            tools,
            tool_choice,
            strict: settings.raise_on_compaction_failure,
            ..CompactionPlan::default()
        }
    }
}

impl Compactor for ProviderSessionCompactor {
    fn compact<'a>(
        &'a self,
        current_session_id: &'a str,
        messages: &'a [ModelMessage],
    ) -> vibe_core::engine::CompactionFuture<'a> {
        Box::pin(async move {
            self.compact_with_instructions(current_session_id, messages, "")
                .await
        })
    }

    fn cleared_session_id(&self, current_session_id: &str) -> Result<String, String> {
        Ok(rotate_session_id(current_session_id))
    }
}

/// The session's own view of the registry: the two configured filters, plus the
/// exact names a subagent is confined to.
///
/// The filters are compiled once here rather than per call, because the
/// reference matching rules cover globs and regular expressions and a turn
/// consults them for every published name and again for every call.
#[derive(Clone)]
pub(super) struct SessionToolExecutor {
    tools: ToolRegistry,
    enabled: NameFilter,
    disabled: NameFilter,
    allowed: Option<BTreeSet<String>>,
}

impl SessionToolExecutor {
    pub(super) fn new(tools: ToolRegistry, intent: &SessionIntent) -> Self {
        Self {
            tools,
            enabled: NameFilter::new(&intent.enabled_tools),
            disabled: NameFilter::new(&intent.disabled_tools),
            allowed: None,
        }
    }

    pub(super) fn with_allowed_tools(mut self, allowed: BTreeSet<String>) -> Self {
        self.allowed = Some(allowed);
        self
    }

    fn permits(&self, name: &str) -> bool {
        (self.enabled.is_empty() || self.enabled.matches(name))
            && !self.disabled.matches(name)
            && self
                .allowed
                .as_ref()
                .is_none_or(|allowed| allowed.contains(name))
    }

    pub(super) fn definitions(&self) -> Result<Vec<ToolDefinition>, DriverError> {
        self.tools
            .available(&self.enabled, &self.disabled)
            .map_err(|error| DriverError::Tool(error.to_string()))
            .map(|definitions| {
                definitions
                    .into_iter()
                    .filter(|definition| self.permits(&definition.name))
                    .map(|spec| ToolDefinition {
                        name: spec.name,
                        description: spec.description,
                        input_schema: spec.input_schema,
                    })
                    .collect()
            })
    }
}

impl ToolExecutor for SessionToolExecutor {
    fn execute<'a>(&'a self, name: &'a str, arguments: &'a str) -> ToolFuture<'a> {
        if !self.permits(name) {
            return Box::pin(
                async move { Err(format!("tool `{name}` is disabled for this session")) },
            );
        }
        self.tools.execute(name, arguments)
    }

    fn execute_stream<'a>(
        &'a self,
        name: &'a str,
        arguments: &'a str,
        output: ToolStreamSink,
    ) -> ToolFuture<'a> {
        if !self.permits(name) {
            return Box::pin(
                async move { Err(format!("tool `{name}` is disabled for this session")) },
            );
        }
        self.tools.execute_stream(name, arguments, output)
    }
}

#[derive(Debug, Clone, Default)]
struct LiveTurnControl {
    cancellation: CancellationToken,
    engine: TurnControlHandle,
}

impl LiveTurnDriver {
    #[cfg(any(test, feature = "test-fixtures"))]
    #[must_use]
    pub fn from_provider_for_tests(
        provider: Arc<dyn CompletionProvider>,
        system_prompt: impl Into<String>,
    ) -> Self {
        let compactor = ProviderSessionCompactor::new(provider.clone());
        Self {
            provider,
            compactor,
            system_prompt: system_prompt.into(),
            session_root: None,
            input_price_per_million_micros: 0,
            output_price_per_million_micros: 0,
            controls: Mutex::new(HashMap::new()),
            pending_context: Mutex::new(HashMap::new()),
            context_warnings: Mutex::new(HashMap::new()),
            event_observer: Arc::new(NoopEventObserver),
        }
    }

    #[cfg(any(test, feature = "test-fixtures"))]
    #[must_use]
    pub fn with_session_root_for_tests(mut self, session_root: Option<PathBuf>) -> Self {
        self.session_root = session_root;
        self
    }

    /// Builds the driver from the ambient credential: the process environment
    /// first, then the variables `dotenv` read from the global file.
    pub fn from_environment(
        config: LiveDriverConfig,
        dotenv: &vibe_core::config::DotenvValues,
    ) -> Result<Self, DriverError> {
        let credential = dotenv
            .variable(&config.credential_environment)
            .filter(|credential| !credential.is_empty())
            .ok_or_else(|| {
                DriverError::MissingCredentialEnvironment(config.credential_environment.clone())
            })?;
        Self::from_credential(config, credential)
    }

    pub fn from_credential(
        config: LiveDriverConfig,
        credential: String,
    ) -> Result<Self, DriverError> {
        let style = ProviderStyle::parse(&config.style).map_err(DriverError::Provider)?;
        if credential.is_empty() {
            return Err(DriverError::MissingCredentialEnvironment(
                config.credential_environment,
            ));
        }
        let transport = HttpTransport::new().map_err(DriverError::Transport)?;
        let provider = ProviderBackend::new(
            style,
            config.endpoint,
            config.model,
            SecretString::from(credential),
            transport,
        );
        let provider: Arc<dyn CompletionProvider> = Arc::new(provider);
        let compactor = ProviderSessionCompactor::new(provider.clone()).with_plan(CompactionPlan {
            prompts: config.compaction_prompts,
            ..CompactionPlan::default()
        });
        let session_root = config.session_root.or_else(default_session_root);
        Ok(Self {
            provider,
            compactor,
            system_prompt: config.system_prompt,
            session_root,
            input_price_per_million_micros: config.input_price_per_million_micros,
            output_price_per_million_micros: config.output_price_per_million_micros,
            controls: Mutex::new(HashMap::new()),
            pending_context: Mutex::new(HashMap::new()),
            context_warnings: Mutex::new(HashMap::new()),
            event_observer: Arc::new(NoopEventObserver),
        })
    }

    #[must_use]
    pub fn with_event_observer(mut self, observer: Arc<dyn EventObserver>) -> Self {
        self.event_observer = observer;
        self
    }

    /// Serves the completions MCP servers ask this client for.
    ///
    /// Reference `_create_sampling_handler` builds one from the loop's backend
    /// and the active model, so a server that asks is answered by the same
    /// provider the turn itself uses rather than by a second one configured
    /// beside it.
    #[must_use]
    pub fn sampling_handler(&self, model: impl Into<String>) -> Arc<dyn SamplingHandler> {
        Arc::new(ProviderSamplingHandler {
            provider: Arc::clone(&self.provider),
            model: model.into(),
        })
    }

    async fn run_engine(
        &self,
        reservation: &TurnReservation,
        cancellation: CancellationToken,
        controls: TurnControlHandle,
        observer: Arc<dyn EventObserver>,
    ) -> Result<TurnOutcome, DriverError> {
        let observer: Arc<dyn EventObserver> = Arc::new(CompositeEventObserver::new(
            observer,
            Arc::clone(&self.event_observer),
        ));
        // Reference `self.agent_profile.name`, which every request and tool
        // event reports and which defaults to the built-in profile.
        let agent_profile = reservation
            .intent
            .agent
            .clone()
            .unwrap_or_else(|| vibe_core::engine::DEFAULT_AGENT_PROFILE.to_owned());
        let limits = EngineLimits {
            max_steps: reservation.intent.max_turns.unwrap_or(20),
            max_total_tokens: reservation.intent.max_tokens.unwrap_or(200_000),
            max_price_micros: reservation.intent.max_price_micros.unwrap_or(u64::MAX),
            input_price_per_million_micros: self.input_price_per_million_micros,
            output_price_per_million_micros: self.output_price_per_million_micros,
            ..EngineLimits::default()
        };
        // The transcript is opened before the history is composed, not after:
        // what the store returns is the history of this turn, so a preamble
        // pushed onto the messages first would be dropped by every cycle but
        // the first one.
        let transcript = match &self.session_root {
            Some(root) => Some(self.open_transcript(root, reservation)?),
            None => None,
        };
        // The subagent tool names the parent session its children fork from, so
        // it is registered before the definitions are read, and only where a
        // store exists for a child to persist into.
        if let Some(transcript) = &transcript {
            self.register_task_tool(
                reservation,
                transcript.store.clone(),
                transcript.session_id.clone(),
            )?;
        }
        let mut messages = self.system_preamble(reservation).await?;
        if let Some(transcript) = &transcript {
            messages = transcript.messages.clone();
        }
        let pending_context = self
            .pending_context
            .lock()
            .map_err(|_| DriverError::StatePoisoned)?
            .remove(&reservation.session_id)
            .unwrap_or_default();
        for entry in pending_context {
            messages.push(ModelMessage::user(entry.content.clone()));
            if entry.inject_invoked_skill
                && let Some(resolver) = reservation.tools.invoked_skills()
                && let Some(invoked) = resolver.resolve(&entry.content)
            {
                vibe_core::skills::append_invoked_skill(&mut messages, &invoked);
            }
        }
        messages.extend(
            resource_contexts(reservation)
                .into_iter()
                .map(ModelMessage::user),
        );
        let session_tools =
            SessionToolExecutor::new(reservation.tools.clone(), &reservation.intent);
        let input = ProviderInput {
            turn_id: Some(reservation.turn_id.clone()),
            model_override: reservation.intent.model.clone(),
            messages,
            stream: true,
            images: match &reservation.prepared_images {
                Some(images) => images.as_slice().to_vec(),
                None => provider_images(&reservation.input)
                    .await?
                    .as_slice()
                    .to_vec(),
            },
            tools: session_tools.definitions()?,
            tool_choice: None,
            thinking: reservation.intent.thinking,
            reasoning_effort: reservation.intent.reasoning_effort.clone(),
            headers: BTreeMap::new(),
            limits: RequestLimits {
                max_tokens: reservation
                    .intent
                    .max_tokens
                    .and_then(|value| u32::try_from(value).ok())
                    .unwrap_or(4096),
                temperature_millis: None,
                max_response_bytes: limits.max_response_bytes,
            },
            metadata: turn_metadata(reservation),
        };
        let (sink, baseline, engine_session_id) = match transcript {
            Some(transcript) => (
                Some(SessionTranscriptSink::new(
                    transcript.store,
                    transcript.metadata.clone(),
                )),
                session_stats(&transcript.metadata),
                transcript.session_id,
            ),
            None => (
                None,
                SessionStats::default(),
                reservation.session_id.clone(),
            ),
        };
        let mut engine = ConversationEngine::new(Arc::clone(&self.provider))
            .with_tools(session_tools)
            .with_compactor(self.compactor.with_plan(self.compactor.session_plan(
                &reservation.compaction,
                input.tools.clone(),
                input.tool_choice.clone(),
                input.thinking,
            )))
            .with_sink(sink)
            .with_limits(limits)
            .with_baseline(baseline)
            .with_compaction_settings(reservation.compaction.clone())
            .with_agent_profile(agent_profile)
            .with_observer(observer);
        // Registered after automatic compaction and before nothing, which is
        // where `_setup_middleware` puts it: a cycle that reached the threshold
        // compacts instead of warning about a window it is about to replace.
        if let Some(warning) =
            self.context_warning(&reservation.session_id, &reservation.compaction)?
        {
            engine = engine.with_middleware(warning);
        }
        if let Some(resolver) = reservation.tools.invoked_skills() {
            engine = engine.with_invoked_skills(resolver);
        }
        engine
            .run_turn_controlled(
                engine_session_id,
                input,
                &reservation.prompt,
                cancellation,
                controls,
            )
            .await
            .map_err(DriverError::Engine)
    }

    /// The system messages every cycle of this session opens with.
    ///
    /// Reference `_loop.py` composes them on every cycle rather than only on
    /// the first, so a resumed session runs under the same directives a fresh
    /// one does. Plan mode also creates the file it names here, which is what
    /// makes the path writable by the time the model reads the directive.
    async fn system_preamble(
        &self,
        reservation: &TurnReservation,
    ) -> Result<Vec<ModelMessage>, DriverError> {
        let mut messages = vec![ModelMessage::System {
            content: self.system_prompt.clone(),
        }];
        if reservation.intent.mode.as_deref() == Some("plan") {
            let plan_path = self
                .plan_directory()
                .map(|directory| plan_file_path(&directory, &reservation.session_id))
                .ok_or_else(|| DriverError::Tool("plan file root is unavailable".to_owned()))?;
            if let Some(parent) = plan_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|error| DriverError::Tool(error.to_string()))?;
            }
            tokio::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&plan_path)
                .await
                .map_err(|error| DriverError::Tool(error.to_string()))?;
            messages.push(ModelMessage::System {
                content: format!(
                    "Plan mode is active. Inspect and reason, but do not mutate the workspace. \
                     Keep the live plan at {} updated as you plan. That plan file is the only \
                     file you may write while plan mode is active.",
                    plan_path.display()
                ),
            });
        }
        if let Some(profile_prompt) = reservation
            .intent
            .system_prompt_id
            .as_deref()
            .and_then(crate::builtin_agents::system_prompt)
        {
            messages.push(ModelMessage::System {
                content: profile_prompt.to_owned(),
            });
        }
        Ok(messages)
    }

    /// Opens the transcript this turn continues, creating one when the store
    /// holds none.
    ///
    /// A `resume` selector and a `continue` both name a session the store
    /// already has. Without either, the session's own identifier is tried and a
    /// miss means this is its first turn.
    fn open_transcript(
        &self,
        root: &Path,
        reservation: &TurnReservation,
    ) -> Result<TurnTranscript, DriverError> {
        let store = SessionStore::new(root);
        let hydrated = if let Some(selector) = &reservation.intent.resume {
            Some(
                store
                    .resume(selector, &self.system_prompt, BTreeMap::new())
                    .map_err(DriverError::Storage)?,
            )
        } else if reservation.intent.continue_session {
            Some(
                store
                    .continue_session(
                        &reservation.working_directory,
                        &self.system_prompt,
                        BTreeMap::new(),
                    )
                    .map_err(DriverError::Storage)?,
            )
        } else {
            match store.resume(
                &reservation.session_id,
                &self.system_prompt,
                BTreeMap::new(),
            ) {
                Ok(hydrated) => Some(hydrated),
                Err(vibe_core::storage::StorageError::SessionNotFound(_)) => None,
                Err(error) => return Err(DriverError::Storage(error)),
            }
        };
        match hydrated {
            Some(hydrated) => Ok(TurnTranscript {
                store,
                session_id: hydrated.metadata.id.clone(),
                metadata: hydrated.metadata,
                messages: hydrated.messages,
            }),
            None => {
                let metadata = store
                    .create(
                        &reservation.session_id,
                        &reservation.working_directory,
                        None,
                        crate::host::now_millis(),
                    )
                    .map_err(DriverError::Storage)?;
                Ok(TurnTranscript {
                    store,
                    metadata,
                    session_id: reservation.session_id.clone(),
                    messages: Vec::new(),
                })
            }
        }
    }
}

/// The client-supplied identifiers a provider request carries alongside the
/// conversation, each present only when the client sent it.
fn turn_metadata(reservation: &TurnReservation) -> BTreeMap<String, String> {
    [
        (
            "client_user_message_id",
            reservation
                .client_user_message_id
                .as_ref()
                .map(|value| json!(value)),
        ),
        (
            "auto_title",
            reservation.auto_title.as_ref().map(|value| json!(value)),
        ),
        (
            "user_display_content",
            reservation.user_display_content.clone(),
        ),
        ("mention_stats", reservation.mention_stats.clone()),
    ]
    .into_iter()
    .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_string())))
    .collect()
}

fn resource_contexts(reservation: &TurnReservation) -> Vec<String> {
    reservation
        .input
        .iter()
        .filter_map(|block| {
            let PublicContentBlock::Resource { resource } = block else {
                return None;
            };
            let embedded = resource.get("resource").unwrap_or(resource);
            let uri = embedded
                .get("uri")
                .or_else(|| resource.get("uri"))
                .and_then(Value::as_str)
                .unwrap_or("attached resource");
            let name = embedded
                .get("name")
                .or_else(|| resource.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(uri);
            let text = embedded
                .get("text")
                .or_else(|| resource.get("text"))
                .and_then(Value::as_str);
            Some(text.map_or_else(
                || format!("Attached resource `{name}` is available at {uri}."),
                |text| format!("Attached resource `{name}` ({uri}):\n{text}"),
            ))
        })
        .collect()
}

impl TurnDriver for LiveTurnDriver {
    fn plan_directory(&self) -> Option<PathBuf> {
        self.session_root
            .as_deref()
            .map(|root| root.parent().unwrap_or(root).join("plans"))
    }

    fn run<'a>(&'a self, reservation: &'a TurnReservation) -> DriverFuture<'a> {
        self.run_observed(reservation, Arc::new(NoopEventObserver))
    }

    fn run_observed<'a>(
        &'a self,
        reservation: &'a TurnReservation,
        observer: Arc<dyn EventObserver>,
    ) -> DriverFuture<'a> {
        Box::pin(async move {
            let key = (reservation.session_id.clone(), reservation.turn_id.clone());
            let control = self
                .controls
                .lock()
                .map_err(|_| DriverError::StatePoisoned)?
                .entry(key.clone())
                .or_default()
                .clone();
            let _registration = ControlRegistration {
                controls: &self.controls,
                key,
            };
            self.run_engine(reservation, control.cancellation, control.engine, observer)
                .await
        })
    }

    fn compact<'a>(
        &'a self,
        session_id: &'a str,
        extra_instructions: &'a str,
    ) -> CompactionDriverFuture<'a> {
        Box::pin(async move {
            let root = self
                .session_root
                .as_ref()
                .ok_or(DriverError::UnsupportedControl("session/compact/start"))?;
            let store = SessionStore::new(root);
            let hydrated = store
                .resume(
                    session_id,
                    &self.system_prompt,
                    BTreeMap::<String, Value>::new(),
                )
                .map_err(DriverError::Storage)?;
            let compaction = self
                .compactor
                .compact_with_instructions(
                    &hydrated.metadata.id,
                    &hydrated.messages,
                    extra_instructions,
                )
                .await
                .map_err(|failure| DriverError::Compaction(failure.message))?;
            store
                .handoff_messages(
                    &hydrated.metadata,
                    &compaction.new_session_id,
                    &compaction.messages,
                    crate::host::now_millis(),
                    // A manual compaction is still a compaction, so the session
                    // it came from stays its parent.
                    true,
                )
                .map_err(DriverError::Storage)?;
            let compacted = store
                .load(&compaction.new_session_id)
                .map_err(DriverError::Storage)?;
            Ok(SessionCompaction {
                old_session_id: hydrated.metadata.id,
                new_session_id: compaction.new_session_id,
                summary: compaction.summary,
                hydrated: compacted,
            })
        })
    }

    fn interrupt(&self, session_id: &str, turn_id: &str) -> Result<(), DriverError> {
        let mut controls = self
            .controls
            .lock()
            .map_err(|_| DriverError::StatePoisoned)?;
        let control = controls
            .entry((session_id.to_owned(), turn_id.to_owned()))
            .or_default();
        control.cancellation.cancel();
        Ok(())
    }

    fn steer(
        &self,
        session_id: &str,
        turn_id: &str,
        content: &str,
        inject_invoked_skill: bool,
    ) -> Result<(), DriverError> {
        self.send_control(
            session_id,
            turn_id,
            TurnControl::Steer {
                content: content.to_owned(),
                inject_invoked_skill,
            },
        )
    }

    fn inject_context(
        &self,
        session_id: &str,
        content: &str,
        as_message: bool,
        inject_invoked_skill: bool,
    ) -> Result<(), DriverError> {
        self.pending_context
            .lock()
            .map_err(|_| DriverError::StatePoisoned)?
            .entry(session_id.to_owned())
            .or_default()
            .push(PendingContext {
                content: content.to_owned(),
                // The reference injects a skill only into a real user turn, so
                // the flag is honored when the entry is one.
                inject_invoked_skill: as_message && inject_invoked_skill,
            });
        Ok(())
    }

    fn resolve_callback(
        &self,
        session_id: &str,
        turn_id: &str,
        callback_id: &str,
        accepted: bool,
        value: Option<&str>,
    ) -> Result<(), DriverError> {
        self.send_control(
            session_id,
            turn_id,
            TurnControl::ResolveCallback {
                callback_id: callback_id.to_owned(),
                accepted,
                value: value.map(str::to_owned),
            },
        )
    }

    fn clear_context(
        &self,
        session_id: &str,
        turn_id: &str,
        continuation: &str,
        plan_file_path: Option<&str>,
    ) -> Result<(), DriverError> {
        self.send_control(
            session_id,
            turn_id,
            TurnControl::ClearContext {
                continuation: continuation.to_owned(),
                plan_file_path: plan_file_path.map(str::to_owned),
            },
        )
    }
}

impl LiveTurnDriver {
    /// The warning policy this session speaks through, created on its first
    /// turn and kept afterward.
    ///
    /// `context_warnings` decides whether the policy is registered at all, the
    /// way the reference's `_setup_middleware` does, rather than registering a
    /// silent one: an unregistered policy cannot latch, so turning the key off
    /// mid-session leaves nothing behind.
    fn context_warning(
        &self,
        session_id: &str,
        settings: &CompactionSettings,
    ) -> Result<Option<Arc<ContextWarningMiddleware>>, DriverError> {
        if !settings.context_warnings {
            return Ok(None);
        }
        let mut warnings = self
            .context_warnings
            .lock()
            .map_err(|_| DriverError::StatePoisoned)?;
        Ok(Some(Arc::clone(
            warnings.entry(session_id.to_owned()).or_default(),
        )))
    }

    fn send_control(
        &self,
        session_id: &str,
        turn_id: &str,
        command: TurnControl,
    ) -> Result<(), DriverError> {
        let mut controls = self
            .controls
            .lock()
            .map_err(|_| DriverError::StatePoisoned)?;
        controls
            .entry((session_id.to_owned(), turn_id.to_owned()))
            .or_default()
            .engine
            .send(command)
            .map_err(DriverError::Engine)
    }
}

struct ControlRegistration<'a> {
    controls: &'a Mutex<HashMap<(String, String), LiveTurnControl>>,
    key: (String, String),
}

impl Drop for ControlRegistration<'_> {
    fn drop(&mut self) {
        if let Ok(mut controls) = self.controls.lock() {
            controls.remove(&self.key);
        }
    }
}
fn default_session_root() -> Option<PathBuf> {
    Some(crate::host::vibe_home().join("sessions"))
}

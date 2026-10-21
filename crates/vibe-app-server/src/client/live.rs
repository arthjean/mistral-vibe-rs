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

use serde_json::{Value, json};

use vibe_core::compaction::CompactionFailure;
use vibe_core::compaction::manager::{
    self as compaction_manager, CompactionPlan, CompactionPromptResolution,
};
use vibe_core::engine::{
    CancellationToken, CompactionResult, Compactor, CompletionProvider, CompositeEventObserver,
    ConversationEngine, EngineLimits, EventObserver, NoopEventObserver, SessionStats,
    SessionTranscriptSink, SummaryRequestRecorder, ToolExecutor, ToolFuture, ToolStreamSink,
    TurnControl, TurnControlHandle, TurnOutcome, TurnStopReason,
};
use vibe_core::events::{ModelMessage, ProjectionReducer, RemoteToolOrigin};
use vibe_core::extensions::{
    AgentKind, AgentProfile, ChildContext, ChildLoggingPolicy, DelegationRequest, DelegationSignal,
    DelegationUpdate, ExtensionSource, SubagentFuture, SubagentManager, SubagentRun,
    SubagentRunner,
};
use vibe_core::llm::availability::ModelAvailability;
use vibe_core::llm::completion::LlmCompletion;
use vibe_core::llm::utility::{self, UtilityFeature, UtilitySelection};
use vibe_core::llm::{AmbientCredentials, BackendContext, Credentials, MapCredentials};
use vibe_core::matching::NameFilter;
use vibe_core::mcp::{
    McpError, McpFuture, SamplingHandler, SamplingRequest, SamplingResponse, SamplingRole,
};
use vibe_core::middleware::{CompactionSettings, ContextWarningMiddleware, PlanAgentMiddleware};
use vibe_core::policy::{PolicyGuardedTool, resolve_task_tool_permission};
use vibe_core::provider::config::UtilityModels;
use vibe_core::provider::config::{ApiSettings, ModelConfig, ModelRouting, ProviderConfig};
use vibe_core::provider::{
    ProviderError, ProviderInput, RequestLimits, ToolChoice, ToolDefinition,
};
use vibe_core::schema::{ObjectSchema, Property};
use vibe_core::session_id::rotate_session_id;
use vibe_core::storage::SessionStore;
use vibe_core::sync::lock;
use vibe_core::tools::config::SharedToolConfig;
use vibe_core::tools::{
    OwnedToolHandlerFuture, ToolAvailability, ToolHandler, ToolRegistry, ToolSource, ToolSpec,
};

pub(super) mod compactor;
pub(crate) mod delegation;
pub(super) mod executor;
mod middleware;
mod titles;
mod transcript;

use compactor::ProviderSessionCompactor;
use executor::SessionToolExecutor;
use titles::{TitleRouting, title_routing};

use delegation::ProviderSamplingHandler;

use super::interactive::plan_review::{PLAN_AGENT, plan_file_path};
use super::{
    CompactionDriverFuture, DriverError, DriverFuture, PublicContentBlock, SessionCompaction,
    SessionIntent, TurnDriver, TurnReservation, provider_images, session_stats,
    snapshot_attachments,
};

#[derive(Debug, Clone)]
pub struct LiveDriverConfig {
    /// The provider turns are sent to. Its `api_key_env_var` names the key,
    /// read from the environment, the global dotenv file, then the keyring.
    pub provider: ProviderConfig,
    /// Every provider the configuration declares, which a background
    /// completion may run on instead (reference `get_mistral_provider`). The
    /// entry named like [`LiveDriverConfig::provider`] is read as that one.
    pub providers: Vec<ProviderConfig>,
    /// The models the configuration declares, which a turn's model name is
    /// resolved against for its temperature, thinking level and image support.
    pub models: Vec<ModelConfig>,
    /// The `allowed_models` patterns, which decide whether the fast utility
    /// model may run a background completion. Reference `_fast_model_allowed`.
    pub allowed_models: Vec<String>,
    /// The `[utility_models]` overrides, which name a title's model outright.
    pub utility_models: UtilityModels,
    /// The model a turn that names none runs on.
    pub model: String,
    /// The request timeout and the retry budget, with the three transport
    /// timeouts. Reference `api_timeout` and its four siblings.
    pub api: ApiSettings,
    /// What a turn opens with when its reservation carries no prompt the
    /// server composed for the session.
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
    /// The plan-agent reminder each session latches, kept across its turns.
    plan_agents: Mutex<HashMap<String, Arc<PlanAgentMiddleware>>>,
    /// The tool surface each session's running turn publishes, which an agent
    /// switch retargets in place.
    turn_tools: Mutex<HashMap<String, SessionToolExecutor>>,
    /// The compactor each session's latest turn summarized under, which a
    /// manual compaction reuses: reference `CompactionManager._primary` sends
    /// the live tool surface whichever path asked for the summary.
    session_compactors: Mutex<HashMap<String, ProviderSessionCompactor>>,
    event_observer: Arc<dyn EventObserver>,
    /// What every request reports as its `metadata`. Reference
    /// `_build_backend_metadata`, read off the same census the session's
    /// telemetry client reports.
    request_census: Option<vibe_core::telemetry::TelemetryContext>,
    /// What background session titles choose their model from (reference
    /// `select_utility_model`), read again for every title since a probe may
    /// have answered meanwhile.
    titles: Option<TitleRouting>,
}

#[derive(Debug, Clone, Default)]
struct LiveTurnControl {
    cancellation: CancellationToken,
    engine: TurnControlHandle,
}

impl LiveTurnDriver {
    /// The provider this driver's turns are sent to, which a Teleport run's
    /// summarization also calls.
    #[must_use]
    pub fn completion_provider(&self) -> Arc<dyn CompletionProvider> {
        Arc::clone(&self.provider)
    }

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
            plan_agents: Mutex::new(HashMap::new()),
            turn_tools: Mutex::new(HashMap::new()),
            session_compactors: Mutex::new(HashMap::new()),
            event_observer: Arc::new(NoopEventObserver),
            request_census: None,
            titles: None,
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
        let credentials = Arc::new(AmbientCredentials::new(
            dotenv.clone(),
            vibe_core::auth::KeyringStore::native(),
        ));
        Self::with_credentials(config, credentials)
    }

    /// Builds the driver around the credential the launch resolved for its
    /// own provider. Any other variable, which a background completion on
    /// another provider reads, resolves as [`LiveTurnDriver::from_environment`]
    /// resolves it.
    pub fn from_credential(
        config: LiveDriverConfig,
        credential: String,
    ) -> Result<Self, DriverError> {
        let credentials = Arc::new(LaunchCredentials {
            launch: MapCredentials([(config.provider.api_key_env_var.clone(), credential)].into()),
            ambient: AmbientCredentials::new(
                vibe_core::config::DotenvValues::global(&crate::host::vibe_home()),
                vibe_core::auth::KeyringStore::native(),
            ),
        });
        Self::with_credentials(config, credentials)
    }

    fn with_credentials(
        config: LiveDriverConfig,
        credentials: Arc<dyn Credentials>,
    ) -> Result<Self, DriverError> {
        let variable = &config.provider.api_key_env_var;
        if !variable.is_empty() && credentials.resolve(variable).is_none() {
            return Err(DriverError::MissingCredentialEnvironment(variable.clone()));
        }
        let titles = Some(TitleRouting {
            routing: title_routing(&config),
            context: BackendContext::ambient(config.api, credentials.clone()),
        });
        let context = BackendContext::ambient(config.api, credentials);
        let provider = LlmCompletion::new(config.provider, config.models, config.model, context)
            .map_err(|failure| {
                DriverError::Provider(ProviderError::InvalidRequest(failure.to_string()))
            })?;
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
            plan_agents: Mutex::new(HashMap::new()),
            turn_tools: Mutex::new(HashMap::new()),
            session_compactors: Mutex::new(HashMap::new()),
            event_observer: Arc::new(NoopEventObserver),
            request_census: None,
            titles,
        })
    }

    #[must_use]
    pub fn with_event_observer(mut self, observer: Arc<dyn EventObserver>) -> Self {
        self.event_observer = observer;
        self
    }

    /// Stamps every request this driver makes with the census `context`
    /// builds.
    #[must_use]
    pub fn with_request_census(mut self, context: vibe_core::telemetry::TelemetryContext) -> Self {
        self.request_census = Some(context);
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
            // An absent budget is no budget: the reference registers a limit
            // policy only for a limit the session was given.
            max_steps: reservation.intent.max_turns.unwrap_or(i64::MAX),
            max_total_tokens: reservation.intent.max_tokens.unwrap_or(i64::MAX),
            max_price_micros: reservation.intent.max_price_micros.unwrap_or(i64::MAX),
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
            // `SessionStore::resume` strips every system entry and reinserts a
            // single one carrying this process's prompt, which the preamble
            // already opens with. Dropping them keeps one copy rather than two.
            messages.extend(
                transcript
                    .messages
                    .iter()
                    .filter(|message| !matches!(message, ModelMessage::System { .. }))
                    .cloned(),
            );
        }
        let pending_context = lock(&self.pending_context)
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
        let session_dir = self
            .session_root
            .as_deref()
            .zip(transcript.as_ref())
            .map(|(root, transcript)| root.join(&transcript.metadata.directory));
        let user_attachments =
            snapshot_attachments(&reservation.input, session_dir.as_deref()).await?;
        let session_tools =
            SessionToolExecutor::new(reservation.tools.clone(), &reservation.intent);
        lock(&self.turn_tools).insert(reservation.session_id.clone(), session_tools.clone());
        let input = ProviderInput {
            dialect: Default::default(),
            turn_id: Some(reservation.turn_id.clone()),
            session_id: None,
            model_override: reservation.intent.model.clone(),
            model: reservation
                .system_prompt
                .as_ref()
                .and_then(|prompt| prompt.model.clone()),
            messages,
            stream: true,
            images: match &reservation.prepared_images {
                Some(images) => images.as_slice().to_vec(),
                None => provider_images(&reservation.input)
                    .await?
                    .as_slice()
                    .to_vec(),
            },
            tools: session_tools.definitions(),
            tool_choice: None,
            thinking: reservation.intent.thinking,
            reasoning_effort: reservation.intent.reasoning_effort.clone(),
            headers: BTreeMap::new(),
            // `--max-tokens` is a session budget (reference
            // `max_session_tokens`), never a cap on one completion: the
            // reference builds its policy with `max_tokens=None`.
            limits: RequestLimits {
                max_tokens: None,
                temperature_millis: None,
            },
            // The provider reads the request census and nothing else; what the
            // client said about its own message stays with the turn.
            metadata: BTreeMap::new(),
        };
        let (sink, baseline, engine_session_id) = match transcript {
            Some(transcript) => (
                Some(
                    SessionTranscriptSink::new(transcript.store, transcript.metadata.clone())
                        .with_pricing(reservation.pricing),
                ),
                session_stats(&transcript.metadata),
                transcript.session_id,
            ),
            None => (
                None,
                SessionStats::default(),
                reservation.session_id.clone(),
            ),
        };
        let compactor = self.compactor.with_plan(self.compactor.session_plan(
            &reservation.compaction,
            input.tools.clone(),
            input.tool_choice.clone(),
            input.thinking,
        ));
        lock(&self.session_compactors).insert(reservation.session_id.clone(), compactor.clone());
        let mut engine = ConversationEngine::new(Arc::clone(&self.provider))
            .with_tools(session_tools)
            .with_compactor(compactor)
            .with_sink(sink)
            .with_limits(limits)
            .with_baseline(baseline)
            .with_compaction_settings(reservation.compaction.clone())
            .with_agent_profile(agent_profile)
            .with_working_directory(&reservation.working_directory)
            .with_observer(observer);
        if let Some(census) = &self.request_census {
            engine = engine.with_request_census(census.clone());
        }
        // Registered after automatic compaction and before nothing, which is
        // where `_setup_middleware` puts it: a cycle that reached the threshold
        // compacts instead of warning about a window it is about to replace.
        if let Some(warning) =
            self.context_warning(&reservation.session_id, &reservation.compaction)
        {
            engine = engine.with_middleware(warning);
        }
        // Registered last, as `_setup_middleware` adds `ReadOnlyAgentMiddleware`.
        engine = engine.with_middleware(self.plan_agent(reservation, &input.tools));
        if let Some(resolver) = reservation.tools.invoked_skills() {
            engine = engine.with_invoked_skills(resolver);
        }
        if reservation.injected {
            engine = engine.with_injected_prompt();
        }
        // Reference `LLMMessage.message_id` defaults to a fresh UUID, so the
        // operator's message never shares an identity with one a resumed
        // session replays from an earlier process.
        engine = engine.with_user_message_id(
            reservation
                .client_user_message_id
                .clone()
                .unwrap_or_else(vibe_core::session_id::uuid_v4),
        );
        // Reference `decode_content_blocks`: attached resources are folded
        // into what the model reads, and the words typed are kept apart for
        // the entry a reload shows.
        let resources = user_attachments
            .iter()
            .filter_map(|block| match block {
                PublicContentBlock::Resource { resource } => Some(resource),
                _ => None,
            })
            .collect::<Vec<_>>();
        let (prompt, input_text) = if resources.is_empty() {
            (reservation.prompt.clone(), None)
        } else {
            (
                vibe_core::events::prompt_with_resources(&reservation.prompt, &resources),
                Some(reservation.prompt.clone()),
            )
        };
        engine = engine
            .with_user_input(input_text, reservation.user_display_content.clone())
            .with_user_attachments(user_attachments);
        // Reference `_hook_session_context`: the transcript is the session's
        // resolved `messages.jsonl` when it is logged, and empty otherwise.
        let transcript_path = session_dir
            .as_deref()
            .map(crate::session_hooks::transcript_path)
            .unwrap_or_default();
        if let Some(hooks) = reservation.hooks.turn_hooks(
            transcript_path,
            reservation.working_directory.clone(),
            None,
        ) {
            engine = engine.with_hooks(hooks);
        }
        engine
            .run_turn_controlled(engine_session_id, input, &prompt, cancellation, controls)
            .await
            .map_err(DriverError::Engine)
    }
}

impl TurnDriver for LiveTurnDriver {
    fn title_model_is_fast(&self) -> Option<bool> {
        self.titles
            .as_ref()
            .and_then(TitleRouting::selection)
            .map(|selection| selection.is_fast())
    }

    fn probe_utility_models(&self) {
        let Some(titles) = self.titles.clone() else {
            return;
        };
        // On a thread of its own, so a caller inside a runtime and one
        // outside it both wait for the answer the same way.
        let probed = std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(utility::ensure_probed(
                &titles.routing,
                &[UtilityFeature::Title],
                None,
                ModelAvailability::global(),
                &titles.context,
            ));
        });
        let _ = probed.join();
    }

    fn generate_title(
        &self,
        session_id: String,
        messages: Vec<ModelMessage>,
        previous_title: Option<String>,
        telemetry: Arc<dyn vibe_core::telemetry::ClientTelemetry>,
    ) -> super::TitleFuture {
        let titles = self.titles.clone();
        // The launch the session's own requests report.
        let launch = self
            .request_census
            .as_ref()
            .and_then(vibe_core::telemetry::TelemetryContext::resolved_launch);
        Box::pin(async move {
            let titles = titles?;
            let selection = titles.selection()?;
            let context = titles.context;
            vibe_core::session_title::generate_session_title(
                &messages,
                previous_title.as_deref(),
                &selection,
                &context,
                utility::Attribution {
                    launch: launch.as_ref(),
                    session_id: Some(&session_id),
                    telemetry: Some(telemetry.as_ref()),
                },
            )
            .await
        })
    }

    /// Reference `PLANS_DIR`: `plans` under the vibe home, which is also
    /// where the plan profile's allowlist lets it write.
    fn summary_provider(&self) -> Option<Arc<dyn CompletionProvider>> {
        Some(self.completion_provider())
    }

    fn plan_directory(&self) -> Option<PathBuf> {
        self.session_root
            .as_ref()
            .map(|_| crate::host::vibe_home().join("plans"))
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
            let control = lock(&self.controls).entry(key.clone()).or_default().clone();
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
        working_directory: &'a str,
        extra_instructions: &'a str,
    ) -> CompactionDriverFuture<'a> {
        Box::pin(async move {
            let root = self
                .session_root
                .as_ref()
                .ok_or(DriverError::UnsupportedControl("session/compact/start"))?;
            let store = SessionStore::new(root);
            // A session no turn has written yet compacts the conversation it
            // would open with, and is written by the compaction, as the
            // reference's agent loop holds its system prompt from the start.
            let hydrated = match store.resume(
                session_id,
                &self.system_prompt,
                BTreeMap::<String, Value>::new(),
            ) {
                Ok(hydrated) => hydrated,
                Err(vibe_core::storage::StorageError::SessionNotFound(_)) => {
                    vibe_core::storage::HydratedSession {
                        metadata: store
                            .create(
                                session_id,
                                working_directory,
                                None,
                                crate::host::now_millis(),
                            )
                            .map_err(DriverError::Storage)?,
                        messages: vec![ModelMessage::System {
                            content: self.system_prompt.clone(),
                        }],
                        current_config: BTreeMap::new(),
                    }
                }
                Err(error) => return Err(DriverError::Storage(error)),
            };
            // Reference `_current_user_message_id`: the request reports the
            // operator's latest message.
            let message_id = hydrated
                .messages
                .iter()
                .rev()
                .find_map(|message| match message {
                    ModelMessage::User {
                        injected: false,
                        message_id,
                        ..
                    } => Some(message_id.clone()),
                    _ => None,
                });
            let requests = Arc::new(SummaryRequestRecorder::new(
                self.request_census.clone(),
                hydrated.metadata.id.clone(),
                message_id.flatten(),
                String::new(),
                self.provider.model().map(ToOwned::to_owned),
            ));
            let compactor = lock(&self.session_compactors)
                .get(session_id)
                .cloned()
                .unwrap_or_else(|| self.compactor.clone());
            let compaction = compactor
                .compact_with_instructions(
                    &hydrated.metadata.id,
                    &hydrated.messages,
                    extra_instructions,
                    Some(compaction_manager::SummaryRequests(requests.clone())),
                )
                .await;
            for (event_id, event) in requests.take().into_iter().enumerate() {
                let envelope = vibe_core::events::EventEnvelope {
                    session_id: hydrated.metadata.id.clone(),
                    turn_id: None,
                    emitted_at: crate::host::now_millis(),
                    working_directory: Some(PathBuf::from(working_directory)),
                    event_id: event_id as u64,
                    event,
                };
                drop(self.event_observer.observe(&envelope));
            }
            let compaction =
                compaction.map_err(|failure| DriverError::Compaction(failure.message))?;
            // The conversation is kept and the envelope written after it,
            // under the same session, and the summarization's usage counts
            // toward the session's own, as every model call of the reference
            // agent loop does.
            let mut metadata = hydrated.metadata.clone();
            for (key, spent) in [
                ("session_prompt_tokens", compaction.usage.input_tokens),
                ("session_completion_tokens", compaction.usage.output_tokens),
            ] {
                let total = metadata
                    .statistics
                    .get(key)
                    .and_then(Value::as_u64)
                    .unwrap_or_default()
                    .saturating_add(spent);
                metadata.statistics.insert(key.to_owned(), json!(total));
            }
            store
                .append_messages(
                    &mut metadata,
                    compaction
                        .messages
                        .get(hydrated.messages.len()..)
                        .unwrap_or_default(),
                    crate::host::now_millis(),
                )
                .map_err(DriverError::Storage)?;
            let compacted = store.open(&metadata.id).map_err(DriverError::Storage)?;
            Ok(SessionCompaction {
                summary: compaction.summary,
                hydrated: compacted,
            })
        })
    }

    fn interrupt(&self, session_id: &str, turn_id: &str) -> Result<(), DriverError> {
        let mut controls = lock(&self.controls);
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
        lock(&self.pending_context)
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

    fn switch_agent(
        &self,
        session_id: &str,
        turn_id: &str,
        profile: &AgentProfile,
        intent: &SessionIntent,
        tools: &ToolRegistry,
    ) -> Result<(), DriverError> {
        let agent_name = profile.name.as_str();
        let definitions = SessionToolExecutor::new(tools.clone(), intent).definitions();
        if let Some(executor) = lock(&self.turn_tools).get(session_id) {
            executor.retarget(intent);
        }
        // Reference `ReadOnlyAgentMiddleware` reads the active profile at the
        // top of the next cycle, which is where leaving plan mode is announced.
        if let Some(plan) = lock(&self.plan_agents).get(session_id) {
            plan.set_active(agent_name == PLAN_AGENT);
        }
        self.send_control(
            session_id,
            turn_id,
            TurnControl::SwitchAgent {
                agent_name: agent_name.to_owned(),
                tools: definitions,
                profile: serde_json::to_value(profile)
                    .map_err(|error| DriverError::Tool(error.to_string()))?,
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
        // Reference `clear_history` resets the middleware, so a cleared
        // conversation hears nothing about the plan mode it left.
        if let Some(plan) = lock(&self.plan_agents).get(session_id) {
            vibe_core::middleware::ConversationMiddleware::reset(
                plan.as_ref(),
                vibe_core::middleware::ResetReason::Compact,
            );
        }
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
    fn send_control(
        &self,
        session_id: &str,
        turn_id: &str,
        command: TurnControl,
    ) -> Result<(), DriverError> {
        let mut controls = lock(&self.controls);
        controls
            .entry((session_id.to_owned(), turn_id.to_owned()))
            .or_default()
            .engine
            .send(command);
        Ok(())
    }
}

struct ControlRegistration<'a> {
    controls: &'a Mutex<HashMap<(String, String), LiveTurnControl>>,
    key: (String, String),
}

impl Drop for ControlRegistration<'_> {
    fn drop(&mut self) {
        lock(self.controls).remove(&self.key);
    }
}

/// The launch's own credential first, then the ambient ones.
struct LaunchCredentials {
    launch: MapCredentials,
    ambient: AmbientCredentials,
}

impl Credentials for LaunchCredentials {
    fn resolve(&self, variable: &str) -> Option<(String, vibe_core::llm::error::KeyOrigin)> {
        self.launch
            .resolve(variable)
            .or_else(|| self.ambient.resolve(variable))
    }
}

/// Where the ambient configuration saves sessions, or `None` when it saves
/// none (reference `SessionLoggingConfig.enabled`).
fn default_session_root() -> Option<PathBuf> {
    crate::workspace::WorkspaceService::default().logged_session_root()
}

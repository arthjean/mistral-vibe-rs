//! The single construction path for the driver, the app server, and the session
//! options, shared by programmatic runs and the interactive TUI.
//!
//! Both entry points used to build these three values independently, so the two
//! modes could drift apart silently. Keeping them here means a provider, backend,
//! or session-intent change lands in one place.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use vibe_app_server::client::{LiveDriverConfig, SessionOptions};
use vibe_app_server::harness::HarnessSelection;
use vibe_app_server::projects::{ProjectsService, VibeCodeCloudConfig};
use vibe_app_server::resources::{
    CoreResourceBackend, MistralConnectorClient, production_mcp_authentication,
    production_mcp_factory,
};
use vibe_app_server::server::{AppServer, WebSearchAccess};
use vibe_app_server::workspace::WorkspaceService;
use vibe_core::config::DotenvValues;
use vibe_core::mcp::SamplingHandler;
use vibe_core::provider::config::{BackendKind, ModelRouting, ProviderConfig};

use secrecy::SecretString;
use url::Url;

use crate::{Arguments, CliError, price_per_million_micros};

const SYSTEM_PROMPT: &str = "You are Mistral Vibe.";

/// The variables `{vibe_home}/.env` declares for this invocation.
///
/// The reference loads that file into the process environment at startup; this
/// port resolves through it instead, because mutating the environment is
/// `unsafe` under edition 2024 and forbidden workspace-wide.
pub(crate) fn dotenv_values(arguments: &Arguments) -> DotenvValues {
    let working_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    DotenvValues::global(&crate::tui::startup::vibe_home_directory(
        arguments,
        &working_directory,
    ))
}

/// The provider this launch sends turns to. See
/// [`ModelRouting::launch_provider`].
pub(crate) fn launch_provider(
    arguments: &Arguments,
    routing: &ModelRouting,
) -> Result<ProviderConfig, CliError> {
    routing
        .launch_provider(
            &arguments.provider_style,
            &arguments.api_base,
            &arguments.credential_environment,
        )
        .ok_or_else(|| {
            CliError::Configuration(format!(
                "`{}` is not a provider style this build speaks",
                arguments.provider_style
            ))
        })
}

/// The providers and models the merged configuration declares.
pub(crate) fn model_routing(workspace: &WorkspaceService) -> Result<ModelRouting, CliError> {
    let snapshot = workspace
        .layered_config()
        .load()
        .map_err(|error| CliError::Configuration(error.to_string()))?;
    Ok(ModelRouting::from_effective(
        &snapshot.effective,
        snapshot.active_model_alias(),
    ))
}

/// The launch arguments this port declares at their defaults: the launch
/// names no provider of its own, so the configuration decides.
pub(crate) const DEFAULT_PROVIDER_STYLE: &str = "mistral";
pub(crate) const DEFAULT_API_BASE: &str = "https://api.mistral.ai/v1";
pub(crate) const DEFAULT_CREDENTIAL_ENVIRONMENT: &str = "MISTRAL_API_KEY";
const DEFAULT_INPUT_PRICE: f64 = 1.5;
const DEFAULT_OUTPUT_PRICE: f64 = 7.5;

/// What a programmatic launch runs on.
///
/// Reference `get_active_model` and `get_provider_for_model`: the model is
/// `active_model`, the provider is the entry that model names, and the prices
/// the price budget counts in are the model's own. The port-only launch
/// arguments still win when they are given, which is what the integration
/// tests and the composite action's self-test point at a stand-in with.
#[derive(Debug, Clone)]
pub(crate) struct ProgrammaticRoute {
    pub(crate) model: String,
    pub(crate) provider: ProviderConfig,
    pub(crate) input_price: f64,
    pub(crate) output_price: f64,
    /// Reference `get_mistral_provider`: the active provider when it is a
    /// Mistral one, else the first Mistral provider configured. `web_search`
    /// reaches the conversations API through it.
    pub(crate) mistral: Option<ProviderConfig>,
}

/// Whether the launch named its provider through the port-only arguments.
fn provider_arguments_given(arguments: &Arguments) -> bool {
    arguments.provider_style != DEFAULT_PROVIDER_STYLE
        || arguments.api_base != DEFAULT_API_BASE
        || arguments.credential_environment != DEFAULT_CREDENTIAL_ENVIRONMENT
}

pub(crate) fn programmatic_route(
    arguments: &Arguments,
    workspace: &WorkspaceService,
) -> Result<ProgrammaticRoute, CliError> {
    let snapshot = workspace
        .layered_config()
        .load()
        .map_err(|error| CliError::Configuration(error.to_string()))?;
    let routing = ModelRouting::from_effective(&snapshot.effective, snapshot.active_model_alias());
    let model = if arguments.model == crate::tui::DEFAULT_MODEL {
        routing
            .active_alias
            .clone()
            .unwrap_or_else(|| arguments.model.clone())
    } else {
        arguments.model.clone()
    };
    let configured = routing.model(Some(&model)).ok();
    let configured_provider = configured
        .as_ref()
        .and_then(|entry| routing.provider_for(entry).ok());
    let provider = match configured_provider {
        Some(provider) if !provider_arguments_given(arguments) => provider,
        _ => launch_provider(arguments, &routing)?,
    };
    let prices = configured
        .as_ref()
        .and_then(|entry| model_prices(&snapshot.effective, &entry.alias));
    let default_prices = arguments.input_price == DEFAULT_INPUT_PRICE
        && arguments.output_price == DEFAULT_OUTPUT_PRICE;
    let (input_price, output_price) = match prices {
        Some(prices) if default_prices => prices,
        _ => (arguments.input_price, arguments.output_price),
    };
    let mistral = if provider.backend == BackendKind::Mistral {
        Some(provider.clone())
    } else {
        routing
            .providers
            .iter()
            .find(|candidate| candidate.backend == BackendKind::Mistral)
            .cloned()
    };
    Ok(ProgrammaticRoute {
        model,
        provider,
        input_price,
        output_price,
        mistral,
    })
}

/// The per-million prices a configured model declares, which default to zero
/// as `ModelConfig.input_price` and `output_price` do.
fn model_prices(effective: &toml::Table, alias: &str) -> Option<(f64, f64)> {
    let entry = match effective.get("models")? {
        toml::Value::Table(models) => models.get(alias)?.as_table()?,
        toml::Value::Array(models) => {
            models
                .iter()
                .filter_map(toml::Value::as_table)
                .find(|entry| {
                    entry
                        .get("alias")
                        .or_else(|| entry.get("name"))
                        .and_then(toml::Value::as_str)
                        == Some(alias)
                })?
        }
        _ => return None,
    };
    let price = |key: &str| {
        entry
            .get(key)
            .and_then(|value| {
                value
                    .as_float()
                    .or_else(|| value.as_integer().map(|n| n as f64))
            })
            .unwrap_or(0.0)
    };
    Some((price("input_price"), price("output_price")))
}

/// The provider's key, from the process environment, the global dotenv file,
/// then the OS keyring, as reference `resolve_api_key` reads it after
/// `load_dotenv_values`. A provider that names no variable needs none.
///
/// # Errors
///
/// Reference `require_active_provider_api_key`: a key none of the three
/// holds refuses the launch.
pub(crate) fn programmatic_credential(
    arguments: &Arguments,
    provider: &ProviderConfig,
) -> Result<String, CliError> {
    let variable = &provider.api_key_env_var;
    if variable.is_empty() {
        return Ok(String::new());
    }
    let working_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let vibe_home = crate::tui::startup::vibe_home_directory(arguments, &working_directory);
    DotenvValues::global(&vibe_home)
        .variable(variable)
        .filter(|credential| !credential.is_empty())
        .or_else(|| {
            crate::tui::setup::PersistedCredentialStore::new(vibe_core::config::global_env_file(
                &vibe_home,
            ))
            .resolve(variable)
        })
        .ok_or_else(|| CliError::MissingApiKey {
            variable: variable.clone(),
            provider: provider.name.clone(),
        })
}

/// The driver a programmatic route runs turns through.
pub(crate) fn route_driver_config(
    route: &ProgrammaticRoute,
    workspace: &WorkspaceService,
) -> Result<LiveDriverConfig, CliError> {
    let routing = model_routing(workspace)?;
    Ok(LiveDriverConfig {
        compaction_prompts: workspace.compaction_prompts(),
        provider: route.provider.clone(),
        models: routing.models,
        model: route.model.clone(),
        api: routing.api,
        system_prompt: SYSTEM_PROMPT.to_owned(),
        session_root: workspace.logged_session_root(),
        input_price_per_million_micros: price_per_million_micros(route.input_price)?,
        output_price_per_million_micros: price_per_million_micros(route.output_price)?,
    })
}

/// [`resource_server`] over the endpoint a programmatic route resolved.
pub(crate) fn route_resource_server(
    arguments: &Arguments,
    workspace: WorkspaceService,
    route: &ProgrammaticRoute,
    credential: String,
    sampling: Option<Arc<dyn SamplingHandler>>,
) -> Result<AppServer, CliError> {
    let mut launch = arguments.clone();
    launch.api_base.clone_from(&route.provider.api_base);
    // Reference `WebSearch.is_available`: the tool is published when the
    // Mistral provider's key resolves, whichever provider runs the turns.
    let web_search = match &route.mistral {
        Some(mistral) if *mistral == route.provider => Some((mistral, credential.clone())),
        Some(mistral) => programmatic_credential(arguments, mistral)
            .ok()
            .map(|key| (mistral, key)),
        None => None,
    }
    .filter(|(_, key)| !key.is_empty())
    .map(|(mistral, key)| {
        let mut target = arguments.clone();
        target.api_base.clone_from(&mistral.api_base);
        web_search_access(&target, key)
    });
    Ok(resource_server(&launch, workspace, credential, sampling)?
        .using_web_search_access(web_search))
}

pub(crate) fn live_driver_config(
    arguments: &Arguments,
    model: &str,
    workspace: &WorkspaceService,
) -> Result<LiveDriverConfig, CliError> {
    let routing = model_routing(workspace)?;
    Ok(LiveDriverConfig {
        compaction_prompts: workspace.compaction_prompts(),
        provider: launch_provider(arguments, &routing)?,
        models: routing.models,
        model: model.to_owned(),
        api: routing.api,
        system_prompt: SYSTEM_PROMPT.to_owned(),
        session_root: workspace.logged_session_root(),
        input_price_per_million_micros: price_per_million_micros(arguments.input_price)?,
        output_price_per_million_micros: price_per_million_micros(arguments.output_price)?,
    })
}

/// The app server with the production resource backend attached. Cloud support
/// is not attached here: each entry point decides whether it needs it, because
/// requiring a usable cloud configuration where none is used would fail a run
/// that never touches the cloud.
pub(crate) fn resource_server(
    arguments: &Arguments,
    workspace: WorkspaceService,
    credential: String,
    sampling: Option<Arc<dyn SamplingHandler>>,
) -> Result<AppServer, CliError> {
    let connector = Arc::new(
        MistralConnectorClient::new(&arguments.api_base, credential.clone())
            .map_err(|error| CliError::Terminal(error.to_string()))?,
    );
    // The sampling handler is what turns an entry's `sampling_enabled` into a
    // capability: it carries the provider the driver already runs turns on, so
    // a server that asks for a completion is answered by the active model.
    let resource_backend = CoreResourceBackend::default()
        .with_config(workspace.layered_config())
        .with_mcp_factory(production_mcp_factory(sampling))
        .with_mcp_authentication(production_mcp_authentication())
        .with_connector_catalog(
            connector.clone(),
            connector.clone(),
            arguments.credential_environment.clone(),
            connector.base_url(),
        )
        .with_connector_auth(connector);
    Ok(AppServer::with_resource_backend(Arc::new(resource_backend))
        .using_workspace_service(workspace)
        .using_web_search_access(Some(web_search_access(arguments, credential)))
        .using_utility_provider(crate::tui::startup::utility_provider(arguments))
        .using_harness_selection(HarnessSelection::resolve(
            arguments.experimental_harness,
            arguments.legacy_harness,
        )))
}

/// The credential and endpoint `web_search` reaches the conversations API with.
///
/// The reference resolves the key from the environment or the OS keyring; the
/// CLI has already done both by the time it builds the server, so it hands the
/// resolved key down rather than letting the server retry only the environment.
fn web_search_access(arguments: &Arguments, credential: String) -> WebSearchAccess {
    // `api_base` is the chat-completions URL, and the conversations endpoint
    // sits at the same origin, which is what the reference derives too.
    let endpoint = Url::parse(&arguments.api_base)
        .ok()
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_else(|| WebSearchAccess::DEFAULT_ENDPOINT.to_owned());
    WebSearchAccess {
        endpoint,
        api_key: SecretString::from(credential),
    }
}

pub(crate) fn cloud_service(credential: String) -> Result<ProjectsService, CliError> {
    let config = VibeCodeCloudConfig::from_credential(credential)
        .map_err(|error| CliError::Teleport(error.to_string()))?;
    ProjectsService::production(config).map_err(|error| CliError::Teleport(error.to_string()))
}

/// Which of the two launches is building the options.
///
/// The reference builds them in two places rather than one, and the
/// programmatic branch is the only one that declares the session headless and
/// withholds the two tools a human would have to answer
/// (`vibe/cli/cli.py:151-192` against `:209-272`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Launch {
    Interactive,
    Programmatic,
}

/// The tools a run with nobody behind it withholds, whatever the user asked
/// for: both of them exist to put a question to a human
/// (`vibe/cli/cli.py:174-178`).
const HEADLESS_WITHHELD_TOOLS: [&str; 2] = ["ask_user_question", "exit_plan_mode"];

/// Session options for one run. `thinking` is derived from `reasoning_effort`
/// rather than passed alongside it, so the two can never disagree.
pub(crate) fn session_options(
    arguments: &Arguments,
    working_directory: &Path,
    model: String,
    mode: Option<String>,
    reasoning_effort: Option<String>,
    launch: Launch,
) -> SessionOptions {
    let headless = launch == Launch::Programmatic;
    // Only a programmatic launch carries the budgets: the reference's
    // interactive session options name none of them (`vibe/cli/cli.py:271-279`).
    let budgets = if headless {
        Budgets::of(arguments)
    } else {
        Budgets::default()
    };
    let (agent, auto_approve) = agent_selection(arguments);
    SessionOptions {
        working_directory: working_directory.to_string_lossy().into_owned(),
        session_id: arguments.resume.clone(),
        add_directories: arguments
            .add_directories
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        trusted: arguments.trust,
        agent,
        tool_filters: arguments.tool_filters.clone(),
        enabled_tools: arguments.enabled_tools.clone(),
        disabled_tools: disabled_tools(&arguments.disabled_tools, headless),
        mcp_servers: Vec::new(),
        model: Some(model),
        max_turns: budgets.max_turns,
        max_tokens: budgets.max_tokens,
        max_price_micros: budgets.max_price_micros,
        mode,
        thinking: reasoning_effort.is_some(),
        reasoning_effort,
        auto_approve,
        headless,
        resume: arguments.resume.clone(),
        continue_session: arguments.continue_session,
    }
}

/// Reference `_agent_selection`: `--auto-approve` without `--agent` selects
/// the `auto-approve` profile rather than approving under the default one.
fn agent_selection(arguments: &Arguments) -> (Option<String>, bool) {
    if arguments.auto_approve && arguments.agent.is_none() {
        return (Some("auto-approve".to_owned()), false);
    }
    (arguments.agent.clone(), arguments.auto_approve)
}

/// The three programmatic budgets, in the units the engine counts in.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Budgets {
    pub(crate) max_turns: Option<i64>,
    pub(crate) max_tokens: Option<i64>,
    pub(crate) max_price_micros: Option<i64>,
}

impl Budgets {
    /// Reads the flags the way the reference's middleware compares them
    /// (`vibe/core/middleware.py:48-96`): signed, so a budget below zero is
    /// one the session has spent before it starts. A price that is not a
    /// number never compares greater, so it sets no budget, and an infinite
    /// one saturates into the value that means none.
    pub(crate) fn of(arguments: &Arguments) -> Self {
        Self {
            max_turns: arguments.max_turns,
            max_tokens: arguments.max_tokens,
            max_price_micros: arguments
                .max_price
                .filter(|price| !price.is_nan())
                .map(|price| (price * 1_000_000.0).round() as i64),
        }
    }
}

/// What the user asked to withhold, plus the two names a headless run adds.
///
/// The reference appends them unconditionally; this port skips a name the user
/// already wrote, because the two lists meet in one `disabled_tools` and a
/// repeated entry would only be matched twice.
fn disabled_tools(requested: &[String], headless: bool) -> Vec<String> {
    let mut names = requested.to_vec();
    if headless {
        for withheld in HEADLESS_WITHHELD_TOOLS {
            if !names.iter().any(|name| name == withheld) {
                names.push(withheld.to_owned());
            }
        }
    }
    names
}

#[cfg(test)]
mod bootstrap_tests;

//! The single construction path for the driver, the app server, and the session
//! options, shared by programmatic runs and the interactive TUI.
//!
//! Both entry points used to build these three values independently, so the two
//! modes could drift apart silently. Keeping them here means a provider, backend,
//! or session-intent change lands in one place.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use vibe_app_server::client::{DriverError, LiveDriverConfig, SessionOptions};
use vibe_app_server::projects::{ProjectsService, VibeCodeCloudConfig};
use vibe_app_server::resources::{
    CoreResourceBackend, MistralConnectorClient, production_mcp_adapters,
};
use vibe_app_server::server::{AppServer, WebSearchAccess};
use vibe_app_server::workspace::WorkspaceService;
use vibe_core::compaction::manager::CompactionPromptResolution;
use vibe_core::config::DotenvValues;
use vibe_core::mcp::SamplingHandler;

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

/// The provider credential: the process environment first, then the global
/// dotenv file.
pub(crate) fn credential(arguments: &Arguments) -> Result<String, DriverError> {
    dotenv_values(arguments)
        .variable(&arguments.credential_environment)
        .ok_or_else(|| {
            DriverError::MissingCredentialEnvironment(arguments.credential_environment.clone())
        })
}

pub(crate) fn live_driver_config(
    arguments: &Arguments,
    model: &str,
    compaction_prompts: CompactionPromptResolution,
) -> Result<LiveDriverConfig, CliError> {
    Ok(LiveDriverConfig {
        compaction_prompts,
        style: arguments.provider_style.clone(),
        endpoint: arguments.api_base.clone(),
        model: model.to_owned(),
        credential_environment: arguments.credential_environment.clone(),
        system_prompt: SYSTEM_PROMPT.to_owned(),
        session_root: arguments.session_root.clone(),
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
    let (mcp_factory, mcp_auth) =
        production_mcp_adapters(sampling).map_err(|error| CliError::Terminal(error.to_string()))?;
    let resource_backend = CoreResourceBackend::default()
        .with_config(workspace.layered_config())
        .with_mcp_factory(mcp_factory)
        .with_mcp_auth(mcp_auth)
        .with_connector_catalog(
            connector.clone(),
            connector.clone(),
            arguments.credential_environment.clone(),
            connector.base_url(),
        )
        .with_connector_auth(connector);
    Ok(AppServer::with_resource_backend(Arc::new(resource_backend))
        .using_workspace_service(workspace)
        .using_web_search_access(Some(web_search_access(arguments, credential))))
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

/// Session options for one run. `thinking` is derived from `reasoning_effort`
/// rather than passed alongside it, so the two can never disagree.
pub(crate) fn session_options(
    arguments: &Arguments,
    working_directory: &Path,
    model: String,
    mode: Option<String>,
    reasoning_effort: Option<String>,
) -> SessionOptions {
    SessionOptions {
        working_directory: working_directory.to_string_lossy().into_owned(),
        session_id: arguments.resume.clone(),
        add_directories: arguments
            .add_directories
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        trusted: arguments.trust,
        agent: arguments.agent.clone(),
        tool_filters: arguments.tool_filters.clone(),
        enabled_tools: arguments.enabled_tools.clone(),
        disabled_tools: arguments.disabled_tools.clone(),
        mcp_servers: Vec::new(),
        model: Some(model),
        max_turns: arguments.max_turns,
        max_tokens: arguments.max_tokens,
        max_price_micros: arguments
            .max_price
            .map(|price| (price * 1_000_000.0).round() as u64),
        mode,
        thinking: reasoning_effort.is_some(),
        reasoning_effort,
        auto_approve: arguments.auto_approve,
        resume: arguments.resume.clone(),
        continue_session: arguments.continue_session,
    }
}

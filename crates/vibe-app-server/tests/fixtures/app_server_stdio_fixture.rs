//! Serves this port's app server over stdio, the way the reference's
//! `vibe-app-server` entry point does, so a differential oracle can drive both
//! through the same script.
//!
//! This is a test fixture, not a shipped binary: the distribution keeps
//! `vibe-app-server` unpublished (row 1 of `docs/parity.md`). It reads what the
//! `vibe-acp` binary reads from its environment: the vibe home, the provider
//! endpoint and the credential variable, which `VIBE_ORACLE_CREDENTIAL`
//! renames from `MISTRAL_API_KEY`.
//!
//! With `VIBE_ORACLE_KEYRING` set, it also serves the MCP catalog, over a
//! credential store kept in that JSON file (`{service: {account: secret}}`),
//! the same file the reference reads through its oracle keyring backend in
//! `scripts/parity/mcp_catalog.py`.
//!
//! A Teleport run summarizes with the driver's own provider, and the Vibe Code
//! links persist under the vibe home. With `VIBE_ORACLE_TELEMETRY` set, the
//! events the server raises itself are delivered the way the reference
//! delivers them, which `scripts/parity/teleport.py` captures; it stays off
//! otherwise, so no other oracle sees a datalake request it never scripted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::BufReader;
use vibe_app_server::client::{LiveDriverConfig, LiveTurnDriver};
use vibe_app_server::projects::ProjectsService;
use vibe_app_server::resources::{CoreResourceBackend, production_mcp_factory};
use vibe_app_server::server::AppServer;
use vibe_app_server::transport::{StdioTransport, serve_stdio};
use vibe_app_server::workspace::WorkspaceService;
use vibe_core::auth::{KeyringBackend, KeyringFailure, McpOAuthStore};
use vibe_core::compaction::manager::CompactionPromptResolution;
use vibe_core::config::DotenvValues;
use vibe_core::config::LayeredConfig;
use vibe_core::mcp::McpAuthenticationService;
use vibe_core::provider::config::{ApiSettings, ProviderConfig};
use vibe_core::telemetry::{
    ClientTelemetry, NoClientTelemetry, TelemetryContext, TelemetryEnvelope, TelemetryRecord,
    merge_properties,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let vibe_home = std::env::var_os("VIBE_HOME")
        .map(PathBuf::from)
        .ok_or("VIBE_HOME must name the fixture's vibe home")?;
    let working_directory = std::env::current_dir()?;
    // The workspace resolves `session_logging` from the configuration, so the
    // driver writes turns where the workspace lists and resumes them.
    let workspace =
        WorkspaceService::for_runtime_session_root(vibe_home.join("sessions"), &working_directory);
    // Reference `vibe-app-server` brings the configuration forward before it
    // composes anything, as the interactive start does.
    workspace.migrate_configuration()?;
    let session_root = workspace
        .persists_runtime_sessions()
        .then(|| workspace.session_root().to_path_buf());
    let dotenv = DotenvValues::global(&vibe_home);
    let style = dotenv
        .variable("VIBE_PROVIDER_STYLE")
        .unwrap_or_else(|| "mistral".to_owned());
    let api_base = dotenv
        .variable("VIBE_API_BASE")
        .ok_or("VIBE_API_BASE must name the scripted completions endpoint")?;
    // A scenario whose active provider is not Mistral names the variable its
    // turns authenticate with, so the fixture starts without a Mistral key as
    // the reference does.
    let credential = dotenv
        .variable("VIBE_ORACLE_CREDENTIAL")
        .unwrap_or_else(|| "MISTRAL_API_KEY".to_owned());
    let config = LiveDriverConfig {
        compaction_prompts: CompactionPromptResolution::default(),
        provider: ProviderConfig::for_style(&style, &api_base, &credential)
            .ok_or("VIBE_PROVIDER_STYLE names no provider style")?,
        models: Vec::new(),
        model: dotenv
            .variable("VIBE_MODEL")
            .unwrap_or_else(|| "mistral-medium-3.5".to_owned()),
        api: ApiSettings::default(),
        system_prompt: "You are Mistral Vibe.".to_owned(),
        session_root,
        input_price_per_million_micros: 1_500_000,
        output_price_per_million_micros: 7_500_000,
    };
    let driver = LiveTurnDriver::from_environment(config, &dotenv)?;
    let projects = ProjectsService::default()
        .with_project_link_store(vibe_home.join("vibe-code-project-links.json"))?;
    let telemetry: Arc<dyn ClientTelemetry> = if std::env::var_os("VIBE_ORACLE_TELEMETRY").is_some()
    {
        Arc::new(LoopbackTelemetry {
            config: workspace.layered_config(),
            dotenv: dotenv.clone(),
            client: reqwest::Client::new(),
        })
    } else {
        Arc::new(NoClientTelemetry)
    };
    let provider = driver.completion_provider();
    let server = match std::env::var_os("VIBE_ORACLE_KEYRING") {
        Some(path) => {
            let store = McpOAuthStore::new(Arc::new(FileKeyring(PathBuf::from(path))), false);
            let backend = CoreResourceBackend::default()
                .with_config(workspace.layered_config())
                .with_mcp_factory(production_mcp_factory(None))
                .with_mcp_authentication(Arc::new(McpAuthenticationService::new(Some(Arc::new(
                    store,
                )))));
            AppServer::with_resource_backend(Arc::new(backend))
        }
        None => AppServer::default(),
    }
    .using_workspace_service(workspace)
    .using_projects_service(projects)
    .using_client_telemetry(telemetry)
    .using_secondary_provider(Some(provider));
    serve_stdio(
        server,
        StdioTransport::new(BufReader::new(tokio::io::stdin()), tokio::io::stdout()),
        Arc::new(driver),
    )
    .await?;
    Ok(())
}

/// Delivers the events the server raises itself to the datalake path on the
/// Mistral provider's own server, gated as the reference gates them: telemetry
/// on, and a Mistral provider whose key resolves. Plain HTTP is accepted here
/// because the oracle's backend is a loopback listener, which the shipped
/// client refuses as a target.
struct LoopbackTelemetry {
    config: LayeredConfig,
    dotenv: DotenvValues,
    client: reqwest::Client,
}

impl ClientTelemetry for LoopbackTelemetry {
    fn record_client_event(
        &self,
        _name: &str,
        _properties: serde_json::Map<String, serde_json::Value>,
        _session_id: Option<&str>,
        _correlate_last_request: bool,
    ) {
    }

    fn record(&self, record: &TelemetryRecord, session_id: Option<&str>) {
        let Ok(snapshot) = self.config.load() else {
            return;
        };
        let effective = &snapshot.effective;
        let enabled = effective
            .get("enable_telemetry")
            .and_then(toml::Value::as_bool)
            .unwrap_or(true);
        let Some(provider) = vibe_core::telemetry::mistral_provider(effective) else {
            return;
        };
        let key = provider
            .get("api_key_env_var")
            .and_then(toml::Value::as_str)
            .and_then(|variable| self.dotenv.variable(variable))
            .filter(|key| !key.is_empty());
        let Some(base) = provider
            .get("api_base")
            .and_then(toml::Value::as_str)
            .and_then(|base| url::Url::parse(base).ok())
        else {
            return;
        };
        let (true, Some(key)) = (enabled, key) else {
            return;
        };
        let Ok(attributes) = record.attributes(None) else {
            return;
        };
        let envelope = TelemetryEnvelope::new(
            record.event().event_name(),
            merge_properties(
                TelemetryContext::default()
                    .base_metadata(session_id)
                    .properties(),
                attributes.into_properties(),
            ),
            None,
        );
        let endpoint = format!("{}/v1/datalake/events", base.origin().ascii_serialization());
        let request = self.client.post(endpoint).bearer_auth(key).json(&envelope);
        tokio::spawn(async move {
            let _ = request.send().await;
        });
    }
}

type Entries = BTreeMap<String, BTreeMap<String, String>>;

/// The oracle's credential store: one JSON file both implementations read.
struct FileKeyring(PathBuf);

impl FileKeyring {
    fn load(&self) -> Result<Entries, KeyringFailure> {
        match std::fs::read_to_string(&self.0) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|error| KeyringFailure::Backend(error.to_string())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Entries::new()),
            Err(error) => Err(KeyringFailure::Backend(error.to_string())),
        }
    }

    fn store(&self, entries: &Entries) -> Result<(), KeyringFailure> {
        let text = serde_json::to_string(entries)
            .map_err(|error| KeyringFailure::Backend(error.to_string()))?;
        std::fs::write(&self.0, text).map_err(|error| KeyringFailure::Backend(error.to_string()))
    }
}

impl KeyringBackend for FileKeyring {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, KeyringFailure> {
        Ok(self
            .load()?
            .get(service)
            .and_then(|accounts| accounts.get(account))
            .cloned())
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> Result<(), KeyringFailure> {
        let mut entries = self.load()?;
        entries
            .entry(service.to_owned())
            .or_default()
            .insert(account.to_owned(), secret.to_owned());
        self.store(&entries)
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), KeyringFailure> {
        let mut entries = self.load()?;
        let removed = entries
            .get_mut(service)
            .and_then(|accounts| accounts.remove(account));
        if removed.is_none() {
            return Err(KeyringFailure::NoEntry);
        }
        self.store(&entries)
    }
}

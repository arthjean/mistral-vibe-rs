//! Serves this port's app server over stdio, the way the reference's
//! `vibe-app-server` entry point does, so a differential oracle can drive both
//! through the same script.
//!
//! This is a test fixture, not a shipped binary: the distribution keeps
//! `vibe-app-server` unpublished (row 1 of `docs/parity.md`). It reads what the
//! `vibe-acp` binary reads from its environment: the vibe home, the provider
//! endpoint and the credential variable.
//!
//! With `VIBE_ORACLE_KEYRING` set, it also serves the MCP catalog, over a
//! credential store kept in that JSON file (`{service: {account: secret}}`),
//! the same file the reference reads through its oracle keyring backend in
//! `scripts/parity/mcp_catalog.py`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::BufReader;
use vibe_app_server::client::{LiveDriverConfig, LiveTurnDriver};
use vibe_app_server::resources::{CoreResourceBackend, production_mcp_factory};
use vibe_app_server::server::AppServer;
use vibe_app_server::transport::{StdioTransport, serve_stdio};
use vibe_app_server::workspace::WorkspaceService;
use vibe_core::auth::{KeyringBackend, KeyringFailure, McpOAuthStore};
use vibe_core::compaction::manager::CompactionPromptResolution;
use vibe_core::config::DotenvValues;
use vibe_core::mcp::McpAuthenticationService;
use vibe_core::provider::config::{ApiSettings, ProviderConfig};

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
    let config = LiveDriverConfig {
        compaction_prompts: CompactionPromptResolution::default(),
        provider: ProviderConfig::for_style(&style, &api_base, "MISTRAL_API_KEY")
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
    .using_workspace_service(workspace);
    serve_stdio(
        server,
        StdioTransport::new(BufReader::new(tokio::io::stdin()), tokio::io::stdout()),
        Arc::new(driver),
    )
    .await?;
    Ok(())
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

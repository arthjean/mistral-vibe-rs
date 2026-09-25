//! Serves this port's app server over stdio, the way the reference's
//! `vibe-app-server` entry point does, so a differential oracle can drive both
//! through the same script.
//!
//! This is a test fixture, not a shipped binary: the distribution keeps
//! `vibe-app-server` unpublished (row 1 of `docs/parity.md`). It reads what the
//! `vibe-acp` binary reads from its environment: the vibe home, the provider
//! endpoint and the credential variable.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::BufReader;
use vibe_app_server::client::{LiveDriverConfig, LiveTurnDriver};
use vibe_app_server::server::AppServer;
use vibe_app_server::transport::{StdioTransport, serve_stdio};
use vibe_app_server::workspace::WorkspaceService;
use vibe_core::compaction::manager::CompactionPromptResolution;
use vibe_core::config::DotenvValues;
use vibe_core::provider::config::{ApiSettings, ProviderConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let vibe_home = std::env::var_os("VIBE_HOME")
        .map(PathBuf::from)
        .ok_or("VIBE_HOME must name the fixture's vibe home")?;
    let session_root = vibe_home.join("sessions");
    let working_directory = std::env::current_dir()?;
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
        session_root: Some(session_root.clone()),
        input_price_per_million_micros: 1_500_000,
        output_price_per_million_micros: 7_500_000,
    };
    let driver = LiveTurnDriver::from_environment(config, &dotenv)?;
    let workspace = WorkspaceService::for_runtime_session_root(session_root, &working_directory);
    let server = AppServer::default().using_workspace_service(workspace);
    serve_stdio(
        server,
        StdioTransport::new(BufReader::new(tokio::io::stdin()), tokio::io::stdout()),
        Arc::new(driver),
    )
    .await?;
    Ok(())
}

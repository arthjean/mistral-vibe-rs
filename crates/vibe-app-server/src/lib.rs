#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

#[cfg(test)]
mod app_server_surface_parity_tests;
mod builtin_agents;
pub mod client;
pub mod client_tools;
mod connector_catalog;
pub mod experiments;
pub mod harness;
mod host;
mod images;
mod live_projection;
mod params;
pub mod projects;
pub mod resources;
pub mod server;
pub mod session_hooks;
mod session_lifecycle;
pub mod startup;
#[cfg(test)]
mod tool_execution_parity_tests;
#[cfg(test)]
mod tool_surface_parity_tests;
pub mod transport;
mod vibe_code;
pub mod vocabulary;
mod wire_validation;
pub mod workspace;
mod worktrees;

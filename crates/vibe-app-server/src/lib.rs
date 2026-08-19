#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

#[cfg(test)]
mod app_server_surface_parity_tests;
mod builtin_agents;
pub mod client;
pub mod client_tools;
pub mod experiments;
mod host;
mod images;
mod live_projection;
mod params;
pub mod projects;
pub mod resources;
pub mod server;
mod session_lifecycle;
pub mod startup;
#[cfg(any(test, feature = "test-fixtures"))]
pub mod streaming_benchmark;
#[cfg(test)]
mod tool_execution_parity_tests;
#[cfg(test)]
mod tool_surface_parity_tests;
pub mod transport;
pub mod vocabulary;
pub mod workspace;

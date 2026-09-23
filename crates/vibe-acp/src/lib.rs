//! Agent Client Protocol adapter over the public app-server contracts.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod agent;
mod auth;
mod client_tools;
mod commands;
mod history;
mod mcp;
mod protocol;
mod session;
mod updates;

#[cfg(test)]
mod tests;

pub use agent::{AcpAgent, AcpExperiments};
pub use auth::{
    AcpAuthEnvironment, AuthAttemptFuture, AuthKeyFuture, ProductionAuthEnvironment,
    default_vibe_home,
};
pub use client_tools::{AcpClientFuture, AcpClientPort, DEFAULT_CLIENT_TOOL_TIMEOUT};
pub use protocol::{
    ACP_PROTOCOL_VERSION, AcpError, AcpForkSession, AcpInitializeRequest, AcpListSessions,
    AcpLoadSession, AcpLoadedSession, AcpNewSession, AcpSessionUpdate,
};

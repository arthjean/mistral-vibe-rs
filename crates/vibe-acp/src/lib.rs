//! Agent Client Protocol adapter over the public app-server contracts.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod agent;
mod auth;
mod client_tools;
mod commands;
mod content;
mod mcp;
mod projection;
mod protocol;
mod router;
mod session;
mod validation;

pub use agent::{AcpAgent, AcpExperiments};
pub use auth::{
    AcpAuthEnvironment, AuthAttemptFuture, AuthKeyFuture, ProductionAuthEnvironment,
    default_vibe_home, setup_command,
};
pub use client_tools::{AcpClientFuture, AcpClientPort, DEFAULT_CLIENT_TOOL_TIMEOUT};
pub use protocol::{
    ACP_PROTOCOL_VERSION, AcpError, AcpForkSession, AcpInitializeRequest, AcpListSessions,
    AcpLoadSession, AcpLoadedSession, AcpNewSession, AcpSessionUpdate,
};
pub use router::Followups;

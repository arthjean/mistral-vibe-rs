//! Agent Client Protocol adapter over the public app-server contracts.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod agent;
mod client_tools;
mod commands;
mod history;
mod mcp;
mod protocol;
mod session;
mod updates;

#[cfg(test)]
mod tests;

pub const ADAPTER_NAME: &str = "vibe-acp";

pub use agent::AcpAgent;
pub use client_tools::{AcpClientFuture, AcpClientPort, DEFAULT_CLIENT_TOOL_TIMEOUT};
pub use protocol::{
    ACP_PROTOCOL_VERSION, AcpAgentCapabilities, AcpClientCapabilities, AcpClientInfo, AcpError,
    AcpFilesystemCapabilities, AcpForkSession, AcpImplementation, AcpInitializeRequest,
    AcpInitializeResponse, AcpListSessions, AcpLoadSession, AcpNewSession, AcpPromptCapabilities,
    AcpPromptResponse, AcpSession, AcpSessionInfo, AcpSessionList, AcpSessionUpdate, AcpUsage,
};
pub use updates::MAX_ACP_UPDATE_QUEUE;

//! What the agent remembers between requests: the client it negotiated with,
//! and the sessions it holds live.

use std::collections::BTreeMap;
use std::sync::Arc;

use vibe_app_server::client::TurnDriver;

use crate::protocol::{AcpClientCapabilities, AcpClientInfo};
use crate::session::AcpHarness;

pub(crate) struct AgentState<D>
where
    D: TurnDriver,
{
    /// What the last `initialize` declared, or nothing before the first one.
    /// The reference serves sessions without a handshake, reading an absent
    /// declaration as a client that hosts nothing.
    pub(crate) client_capabilities: Option<AcpClientCapabilities>,
    pub(crate) client_info: Option<AcpClientInfo>,
    /// Live sessions, keyed by the identity the client addresses them by.
    pub(crate) sessions: BTreeMap<String, Arc<AcpHarness<D>>>,
}

impl<D> AgentState<D>
where
    D: TurnDriver,
{
    pub(crate) const fn new() -> Self {
        Self {
            client_capabilities: None,
            client_info: None,
            sessions: BTreeMap::new(),
        }
    }

    pub(crate) fn capabilities(&self) -> AcpClientCapabilities {
        self.client_capabilities.clone().unwrap_or_default()
    }
}

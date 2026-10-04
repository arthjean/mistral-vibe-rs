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
    /// The identities sessions were opened under, oldest first, so the first
    /// one still live is known (reference `next(iter(self.sessions.values()))`
    /// over an insertion-ordered dict).
    pub(crate) opened: Vec<String>,
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
            opened: Vec::new(),
        }
    }

    /// The live session opened first.
    pub(crate) fn first_session(&self) -> Option<Arc<AcpHarness<D>>> {
        self.opened
            .iter()
            .find_map(|session_id| self.sessions.get(session_id).cloned())
    }

    /// Records a session going live: a new one goes last, one already live
    /// keeps its place, and one closed meanwhile is forgotten.
    pub(crate) fn open(&mut self, harness: Arc<AcpHarness<D>>) {
        let session_id = harness.session_id.clone();
        let live = &self.sessions;
        self.opened.retain(|opened| live.contains_key(opened));
        if !self.sessions.contains_key(&session_id) {
            self.opened.push(session_id.clone());
        }
        self.sessions.insert(session_id, harness);
    }

    pub(crate) fn capabilities(&self) -> AcpClientCapabilities {
        self.client_capabilities.clone().unwrap_or_default()
    }
}

//! Per-session lifecycle state owned by the agent.
//!
//! A single slot per session ID replaces the parallel active, loading,
//! closing, and closed collections the adapter used to reconcile by hand.

use std::collections::BTreeMap;
use std::sync::Arc;

use vibe_app_server::client::TurnDriver;

use crate::protocol::{AcpClientCapabilities, AcpClientInfo, AcpError};
use crate::session::{AcpHarness, now_millis};

/// Closed sessions stay recorded so repeated closes remain idempotent, but the
/// tombstones are evicted oldest-first instead of growing without bound.
pub(crate) const MAX_CLOSED_SESSIONS: usize = 1_024;

/// Lifecycle of one session ID. A single slot replaces the parallel active,
/// loading, closing, and closed collections the agent used to reconcile.
pub(crate) enum SessionSlot<D>
where
    D: TurnDriver,
{
    Loading,
    Active(Arc<AcpHarness<D>>),
    Closing(Arc<AcpHarness<D>>),
    /// Retains the closed identity so repeated closes stay idempotent. The
    /// sequence bounds retention: the oldest tombstone is evicted first.
    Closed(u64),
}

impl<D> SessionSlot<D>
where
    D: TurnDriver,
{
    pub(crate) fn harness(&self) -> Option<&Arc<AcpHarness<D>>> {
        match self {
            Self::Active(harness) | Self::Closing(harness) => Some(harness),
            Self::Loading | Self::Closed(_) => None,
        }
    }
}

pub(crate) struct AgentState<D>
where
    D: TurnDriver,
{
    pub(crate) initialized: bool,
    client_capabilities: AcpClientCapabilities,
    client_info: Option<AcpClientInfo>,
    pub(crate) sessions: BTreeMap<String, SessionSlot<D>>,
    next_session: u64,
    next_tombstone: u64,
}

impl<D> AgentState<D>
where
    D: TurnDriver,
{
    pub(crate) fn new() -> Self {
        Self {
            initialized: false,
            client_capabilities: AcpClientCapabilities::default(),
            client_info: None,
            sessions: BTreeMap::new(),
            next_session: 1,
            next_tombstone: 1,
        }
    }

    /// Records the capabilities negotiated at initialization.
    pub(crate) fn initialize(
        &mut self,
        capabilities: AcpClientCapabilities,
        client_info: Option<AcpClientInfo>,
    ) -> Result<(), AcpError> {
        if self.initialized {
            return Err(AcpError::AlreadyInitialized);
        }
        self.initialized = true;
        self.client_capabilities = capabilities;
        self.client_info = client_info;
        Ok(())
    }

    pub(crate) fn client_capabilities(&self) -> AcpClientCapabilities {
        self.client_capabilities.clone()
    }

    pub(crate) fn client_context(&self) -> (AcpClientCapabilities, Option<AcpClientInfo>) {
        (self.client_capabilities.clone(), self.client_info.clone())
    }

    /// Session identity, unique across concurrent adapters on the same host.
    pub(crate) fn mint_session_id(&mut self) -> String {
        let sequence = self.next_session;
        self.next_session = self.next_session.saturating_add(1);
        format!(
            "acp-session-{:x}-{:x}-{sequence:x}",
            now_millis(),
            std::process::id(),
        )
    }

    pub(crate) fn active(&self, session_id: &str) -> Option<Arc<AcpHarness<D>>> {
        match self.sessions.get(session_id) {
            Some(SessionSlot::Active(harness)) => Some(harness.clone()),
            _ => None,
        }
    }

    pub(crate) fn insert_active(&mut self, session_id: String, harness: Arc<AcpHarness<D>>) {
        self.sessions
            .insert(session_id, SessionSlot::Active(harness));
    }

    /// Claims a session ID for loading so two concurrent loads cannot both
    /// attach the same canonical session.
    pub(crate) fn begin_load(&mut self, session_id: &str) -> Result<(), AcpError> {
        match self.sessions.get(session_id) {
            Some(SessionSlot::Loading | SessionSlot::Active(_) | SessionSlot::Closing(_)) => {
                Err(AcpError::SessionConflict(session_id.to_owned()))
            }
            Some(SessionSlot::Closed(_)) | None => {
                self.sessions
                    .insert(session_id.to_owned(), SessionSlot::Loading);
                Ok(())
            }
        }
    }

    pub(crate) fn abort_load(&mut self, session_id: &str) {
        if matches!(self.sessions.get(session_id), Some(SessionSlot::Loading)) {
            self.sessions.remove(session_id);
        }
    }

    /// Publishes a loaded session, unless the reservation was dropped by a
    /// concurrent disconnect. The two ways that happens are reported apart:
    /// the adapter shut down, or something else claimed the identity while the
    /// load was in flight.
    pub(crate) fn finish_load(
        &mut self,
        reserved_id: &str,
        session_id: String,
        harness: Arc<AcpHarness<D>>,
    ) -> Result<(), AcpError> {
        if !self.initialized {
            return Err(AcpError::NotInitialized);
        }
        if !matches!(self.sessions.get(reserved_id), Some(SessionSlot::Loading)) {
            return Err(AcpError::SessionConflict(reserved_id.to_owned()));
        }
        self.sessions.remove(reserved_id);
        self.insert_active(session_id, harness);
        Ok(())
    }

    /// `Ok(None)` means the session needs no work: it is already closed or a
    /// concurrent close owns it.
    pub(crate) fn begin_close(
        &mut self,
        session_id: &str,
    ) -> Result<Option<Arc<AcpHarness<D>>>, AcpError> {
        match self.sessions.get(session_id) {
            Some(SessionSlot::Closing(_) | SessionSlot::Closed(_)) => Ok(None),
            Some(SessionSlot::Loading) => Err(AcpError::SessionConflict(session_id.to_owned())),
            Some(SessionSlot::Active(harness)) => {
                let harness = harness.clone();
                self.sessions
                    .insert(session_id.to_owned(), SessionSlot::Closing(harness.clone()));
                Ok(Some(harness))
            }
            None => Err(AcpError::SessionNotFound(session_id.to_owned())),
        }
    }

    pub(crate) fn is_closed(&self, session_id: &str) -> bool {
        matches!(self.sessions.get(session_id), Some(SessionSlot::Closed(_)))
    }

    /// Records the outcome of a close: a stopped session becomes a tombstone,
    /// a failed one returns to service.
    pub(crate) fn finish_close(
        &mut self,
        session_id: &str,
        harness: Arc<AcpHarness<D>>,
        stopped: bool,
    ) {
        if stopped {
            self.tombstone(session_id.to_owned());
        } else {
            self.insert_active(session_id.to_owned(), harness);
        }
    }

    pub(crate) fn tombstone(&mut self, session_id: String) {
        let sequence = self.next_tombstone;
        self.next_tombstone = self.next_tombstone.saturating_add(1);
        self.sessions
            .insert(session_id, SessionSlot::Closed(sequence));
        self.evict_oldest_tombstones();
    }

    fn evict_oldest_tombstones(&mut self) {
        let mut tombstones = self
            .sessions
            .iter()
            .filter_map(|(session_id, slot)| match slot {
                SessionSlot::Closed(sequence) => Some((*sequence, session_id.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        if tombstones.len() <= MAX_CLOSED_SESSIONS {
            return;
        }
        tombstones.sort_unstable();
        let excess = tombstones.len().saturating_sub(MAX_CLOSED_SESSIONS);
        for (_, session_id) in tombstones.into_iter().take(excess) {
            self.sessions.remove(&session_id);
        }
    }

    /// Takes ownership of every live session for shutdown, keeping tombstones
    /// so a concurrent close can still observe that its session finished.
    pub(crate) fn take_live_sessions(&mut self) -> Vec<(String, Arc<AcpHarness<D>>)> {
        self.initialized = false;
        let mut live = Vec::new();
        self.sessions.retain(|session_id, slot| match slot {
            SessionSlot::Active(harness) | SessionSlot::Closing(harness) => {
                live.push((session_id.clone(), harness.clone()));
                false
            }
            SessionSlot::Loading => false,
            SessionSlot::Closed(_) => true,
        });
        live
    }
}

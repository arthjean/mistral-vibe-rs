//! The session leases this process holds (reference `SessionLease`), and the
//! guard that gives one back unless the session it was taken for opens.

use super::*;
use vibe_core::storage::lease::{LeaseError, SessionLease};

/// Every lease this process holds, by the session identifier it guards.
pub(in crate::server) type Leases = Arc<Mutex<BTreeMap<String, SessionLease>>>;

/// A lease an opening took, released when the guard drops unless the opening
/// commits it with [`LeaseGuard::keep`].
///
/// A guard over a lease this process held already, or taken while session
/// logging is off, releases nothing: the holder that took it lets it go.
#[must_use = "dropping the guard releases the lease it took"]
pub(in crate::server) struct LeaseGuard {
    taken: Option<(Leases, String)>,
}

impl LeaseGuard {
    /// Takes the lease on `session_id` under `root` into `leases`, or reports
    /// who holds it.
    fn take(leases: &Leases, root: &Path, session_id: &str) -> Result<Self, LeaseError> {
        let mut held = lock(leases);
        if held.contains_key(session_id) {
            return Ok(Self { taken: None });
        }
        held.insert(
            session_id.to_owned(),
            SessionLease::acquire(root, session_id)?,
        );
        Ok(Self {
            taken: Some((Arc::clone(leases), session_id.to_owned())),
        })
    }

    /// Commits the lease to the session it was taken for, which releases it
    /// when it closes.
    pub(in crate::server) fn keep(mut self) {
        self.taken = None;
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if let Some((leases, session_id)) = self.taken.take() {
            let lease = lock(&leases).remove(&session_id);
            drop(lease);
        }
    }
}

impl From<LeaseError> for ProtocolFault {
    fn from(error: LeaseError) -> Self {
        match error {
            LeaseError::Busy(_) => {
                ProtocolFault::plain(ProtocolErrorCode::Conflict, error.to_string())
            }
            error => ProtocolFault::internal(error.to_string()),
        }
    }
}

impl AppServer {
    /// Takes the lease on `session_id` for this process, or reports who holds
    /// it (reference `_acquire_session_lease`). Nothing is leased while session
    /// logging is off, and a session this process holds already stays held.
    pub(in crate::server) fn acquire_lease(
        &self,
        session_id: &str,
    ) -> Result<LeaseGuard, LeaseError> {
        if !self.workspace.session_logging().enabled {
            return Ok(LeaseGuard { taken: None });
        }
        LeaseGuard::take(&self.leases, self.workspace.session_root(), session_id)
    }

    /// Lets `session_id` go, so another process may open it.
    pub(in crate::server) fn release_lease(&self, session_id: &str) {
        let lease = lock(&self.leases).remove(session_id);
        drop(lease);
    }
}

#[cfg(test)]
mod leases_tests;

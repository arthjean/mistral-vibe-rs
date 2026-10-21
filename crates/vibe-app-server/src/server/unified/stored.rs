//! Unified sessions as their stores hold them: listing, reading and deleting
//! one, live or not.
//!
//! Reference `list`, `read` and `delete` on the unified host
//! (`vibe/app_server/_unified_harness_backend_adapter.py`): a session this
//! process holds open answers from its live state, any other from its store.

use super::params::{ListParams, ReadParams};
use super::*;

/// The most recently updated listed session that is not archived.
fn latest_open(items: &[Value]) -> Option<String> {
    items
        .iter()
        .filter(|item| item["archivedAt"].is_null())
        .max_by_key(|item| {
            (
                item["updatedAt"].as_u64(),
                item["id"].as_str().map(ToOwned::to_owned),
            )
        })
        .and_then(|item| item["id"].as_str().map(ToOwned::to_owned))
}

/// The public session a listing shows for a stored unified session.
///
/// Reference `_list` prefers the projection of a session this process holds
/// open (`_live_listed_sessions`), so `live` stands in for the stored one.
fn listed_unified_session(store: &SessionStore, live: Option<&UnifiedSession>) -> Option<Value> {
    let snapshot = store.snapshot()?;
    let meta = store.read_meta().unwrap_or_default();
    let session = match live {
        Some(live) => live.core().projector.state.session.clone(),
        None => {
            let mut session = HarnessState::from_persisted(&snapshot, Vec::new()).session;
            store.replay_journal(&mut session);
            session
        }
    };
    Some(wire_session(
        &session,
        &WireContext {
            cwd: meta_cwd(&meta),
            marks: marks_of(&meta),
            ..WireContext::default()
        },
    ))
}

impl ServerConnection {
    /// The stored unified session `session_id` names, with its directory.
    pub(super) fn stored_unified(&self, session_id: &str) -> Option<SessionStore> {
        let root = self.server.unified_session_root()?;
        SessionStore::open(store::unified_root(&root).join(session_id))
    }

    /// [`Self::stored_unified`], refusing a session no store holds.
    pub(super) fn require_stored(&self, session_id: &str) -> Result<SessionStore, ProtocolFault> {
        self.stored_unified(session_id)
            .ok_or_else(|| unknown_session(session_id))
    }

    /// Every stored unified session, as a listing shows it.
    fn listed_sessions(&self) -> Vec<Value> {
        self.server
            .unified_session_root()
            .map(|root| store::list(&root))
            .unwrap_or_default()
            .iter()
            .filter_map(|store| {
                let id = store.dir().file_name()?.to_str()?;
                listed_unified_session(store, self.server.unified_session(id).as_deref())
            })
            .collect()
    }

    /// Reference `list` on the unified host: the unified sessions merged with
    /// the legacy ones, newest first.
    pub(in crate::server) fn unified_list(
        &self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let params = from_params::<ListParams>(&request.params)?;
        let cwd = params.cwd.map(|cwd| {
            vibe_core::platform::canonicalize(&cwd)
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or(cwd)
        });
        let mut items: Vec<Value> = self
            .listed_sessions()
            .into_iter()
            .filter(|item| {
                cwd.as_ref()
                    .is_none_or(|cwd| item["cwd"].as_str() == Some(cwd))
            })
            .collect();
        let continue_session_id = latest_open(&items).map_or(Value::Null, |id| json!(id));
        // Reference `_merged_session_list`: the sessions an older Vibe saved
        // are listed beside the unified ones, and never continued.
        items.extend(self.legacy_rows(cwd.as_deref()));
        if !params.include_archived {
            items.retain(|item| item["archivedAt"].is_null());
        }
        if let Some(pinned) = params.pinned {
            items.retain(|item| item["pinnedAt"].is_null() != pinned);
        }
        items.sort_by_key(|item| {
            std::cmp::Reverse((
                item["updatedAt"].as_u64(),
                item["id"].as_str().map(ToOwned::to_owned),
            ))
        });
        items.truncate(params.limit);
        Ok(success_batch(
            request.id.clone(),
            result_map([
                ("items", json!(items)),
                ("nextCursor", Value::Null),
                ("previousCursor", Value::Null),
                ("continueSessionId", continue_session_id),
            ]),
        ))
    }

    /// The session `--continue` reopens: reference `_sweep_unified_sessions`
    /// lists without a directory, so it is the latest open one anywhere.
    pub(super) fn unified_continue_target(&self) -> Option<String> {
        latest_open(&self.listed_sessions())
    }

    /// Reference `delete` on the unified host: a live session is closed,
    /// its running turn interrupted, and its store removed. `None` when no
    /// unified session holds `session_id`, which the legacy path answers.
    pub(in crate::server) fn unified_delete(
        &self,
        session_id: &str,
    ) -> Option<Result<(), ProtocolFault>> {
        let live = self.server.remove_unified(session_id);
        if let Some(live) = &live {
            {
                let mut host = live.host();
                if let Some(interrupt) = host.interrupt.take() {
                    let _ = interrupt.send(Some("session deleted".to_owned()));
                }
                host.store = None;
            }
            let core = live.core();
            core.subagents.shut_down();
            let processes = Arc::clone(&core.processes);
            std::thread::spawn(move || processes.shut_down());
        }
        let directory = self
            .server
            .unified_session_root()
            .map(|root| store::unified_root(&root).join(session_id))
            .filter(|directory| directory.is_dir());
        if live.is_none() && directory.is_none() {
            return None;
        }
        if let Some(directory) = directory
            && let Err(error) = std::fs::remove_dir_all(&directory)
        {
            return Some(Err(ProtocolFault::internal(format!(
                "the session store cannot be removed: {error}"
            ))));
        }
        Some(Ok(()))
    }

    /// Reference `read` on the unified host.
    pub(in crate::server) fn unified_read(
        &self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let params = from_params::<ReadParams>(&request.params)?;
        let history_limit = params.history.and_then(|history| history.limit);
        let state = self.unified_state(&params.session_id, history_limit)?;
        Ok(success_batch(
            request.id.clone(),
            result_map([("lastEventId", state["eventId"].clone()), ("state", state)]),
        ))
    }

    /// The directory the unified session `session_id` runs in, live or
    /// stored.
    fn unified_cwd(&self, session_id: &str) -> Option<String> {
        if let Some(live) = self.server.unified_session(session_id) {
            return Some(live.cwd.clone());
        }
        meta_cwd(&self.stored_unified(session_id)?.read_meta()?)
    }

    /// The public state of a unified session, live or stored, with at most
    /// `history_limit` of its latest history entries.
    pub(in crate::server) fn unified_state(
        &self,
        session_id: &str,
        history_limit: Option<usize>,
    ) -> Result<Value, ProtocolFault> {
        if let Some(live) = self.server.unified_session(session_id) {
            return Ok(live.read_state(history_limit));
        }
        let Some(stored) = self.stored_unified(session_id) else {
            return self
                .legacy_state(session_id, history_limit)
                .ok_or_else(|| unknown_session(session_id));
        };
        let core = stored
            .load(PathBuf::new())
            .ok_or_else(|| ProtocolFault::internal("the session store cannot be read"))?;
        let meta = stored.read_meta().unwrap_or_default();
        let session = &core.projector.state.session;
        // A child keeps no metadata of its own: it runs in its root's
        // directory.
        let cwd = meta_cwd(&meta).or_else(|| {
            (session.root_session_id != session.id)
                .then(|| self.unified_cwd(&session.root_session_id))
                .flatten()
        });
        Ok(wire_state(
            &core.projector.state,
            &WireContext {
                cwd,
                event_id: core.projector.watermark,
                history_limit,
                marks: marks_of(&meta),
                pins: stored_pins(&core),
                worktree: None,
                child_sessions: Vec::new(),
            },
        ))
    }
}

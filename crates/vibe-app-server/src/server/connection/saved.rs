//! The methods that address saved sessions: listing and reading them,
//! reopening, forking, renaming, moving, clearing and deleting.
//!
//! The reference routes each one by whether the connection holds a root
//! session (`vibe/app_server/server.py` `_dispatch_to_root` and
//! `_dispatch_without_root`): the live session answers for itself, and the
//! store answers for every other one (`vibe/app_server/_host.py`). A method the
//! host does not serve needs a root, and without one the answer is a conflict.

use super::*;
use crate::params::{WireCheck, object_of};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Map;
use std::fs;
use vibe_core::storage::lease::{LeaseError, SessionLease};
use vibe_core::storage::{SessionInfo, SessionStore, StorageError, parse_iso_millis};
use vibe_core::worktree::WorktreeRepository;

/// Reference `_open_initial_session_locked`: what a method needing a root
/// answers without one.
const NO_ROOT: &str = "Start, resume, or continue a session before using this method";

/// Reference `SessionListParams.limit` and `PageRequest.limit` bounds.
const PAGE_BOUNDS: (i64, i64) = (1, 500);

impl AppServer {
    /// Takes the lease on `session_id` for this process, or reports who holds
    /// it (reference `_acquire_session_lease`). Nothing is leased while session
    /// logging is off, and a session this process holds already stays held.
    pub(super) fn acquire_lease(&self, session_id: &str) -> Result<(), ProtocolFault> {
        if !self.workspace.session_logging().enabled {
            return Ok(());
        }
        let mut leases = self.leases.lock().map_err(|_| ServerError::StatePoisoned)?;
        if leases.contains_key(session_id) {
            return Ok(());
        }
        match SessionLease::acquire(self.workspace.session_root(), session_id) {
            Ok(lease) => {
                leases.insert(session_id.to_owned(), lease);
                Ok(())
            }
            Err(error @ LeaseError::Busy(_)) => Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                error.to_string(),
            )),
            Err(error) => Err(ProtocolFault::internal(error.to_string())),
        }
    }

    /// Lets `session_id` go, so another process may open it.
    pub(super) fn release_lease(&self, session_id: &str) {
        let lease = self
            .leases
            .lock()
            .ok()
            .and_then(|mut leases| leases.remove(session_id));
        drop(lease);
    }

    /// Moves the lease a session holds onto the identifier it continues under.
    pub(super) fn transfer_lease(&self, from: &str, to: &str) -> Result<(), ProtocolFault> {
        self.acquire_lease(to)?;
        self.release_lease(from);
        Ok(())
    }
}

impl ServerConnection {
    pub(super) fn saved_session_request(&mut self, request: ServerRequest) -> DispatchBatch {
        let id = request.id.clone();
        let outcome = match request.method.as_str() {
            "session/list" => self.list_sessions(&request),
            "session/read" => self.read_session(&request),
            "session/history/get" => self.history_get(&request),
            "session/history/list" => self.history_list(&request),
            "session/turns/list" => self.turns_list(&request),
            "session/rename" => self.rename_session(&request),
            "session/title/update" => Err(self.unserved(&request.method)),
            // Pinning belongs to the unified harness; the legacy one declines
            // it before reading anything (`vibe/app_server/server.py:878`).
            "session/pin" => Err(ProtocolFault::plain(
                ProtocolErrorCode::MethodNotFound,
                format!("Method not found: {}", request.method),
            )),
            "session/relocate" => self.relocate_session(&request),
            "session/log/read" => self.read_session_log(&request),
            "session/resume" => self.reopen_session(request, Reopen::Resume),
            "session/continue" => self.reopen_session(request, Reopen::Continue),
            "session/fork" => self.fork_session(&request),
            "session/delete" => self.delete_saved_session(&request),
            "session/history/clear" => self.clear_history(&request),
            _ => Err(ProtocolFault::plain(
                ProtocolErrorCode::MethodNotFound,
                format!("Method not found: {}", request.method),
            )),
        };
        answered(id, outcome)
    }

    /// The registry key of the session this connection is rooted in.
    pub(super) fn root_key(&self) -> Option<String> {
        let root = self.root.as_ref()?;
        let sessions = self.server.lock_sessions().ok()?;
        sessions
            .get(root)
            .filter(|session| session.status != SessionStatus::Closed)
            .map(|_| root.clone())
    }

    /// The identifier the root session answers to now.
    pub(super) fn root_id(&self) -> Option<String> {
        let root = self.root_key()?;
        let sessions = self.server.lock_sessions().ok()?;
        sessions.get(&root).map(|session| session.id.clone())
    }

    /// A method the host does not serve: the root answers it, and without one
    /// the connection has nothing to answer with.
    fn unserved(&self, method: &str) -> ProtocolFault {
        if self.root_key().is_some() {
            ProtocolFault::plain(
                ProtocolErrorCode::MethodNotFound,
                format!("Method not found: {method}"),
            )
        } else {
            ProtocolFault::plain(ProtocolErrorCode::Conflict, NO_ROOT)
        }
    }

    /// The root, which `session_id` has to name (reference
    /// `_require_session`). Without a root the method was never the host's.
    fn require_root(&self, session_id: &str) -> Result<String, ProtocolFault> {
        let Some(root) = self.root_id() else {
            return Err(ProtocolFault::plain(ProtocolErrorCode::Conflict, NO_ROOT));
        };
        if root == session_id {
            Ok(root)
        } else {
            Err(not_found(session_id))
        }
    }

    fn store(&self) -> SessionStore {
        self.server.workspace.session_store()
    }

    // ------------------------------------------------------------- listing

    /// Reference `project_session_list`.
    fn list_sessions(&self, request: &ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let object = object_of(&request.params);
        let mut check = WireCheck::default();
        let cursor = check.optional_string(&object, "cursor", &[]);
        let limit = check.bounded_int(&object, "limit", &[], 50, PAGE_BOUNDS);
        let root_session_id = check.optional_string(&object, "rootSessionId", &[]);
        let parent_session_id = check.optional_string(&object, "parentSessionId", &[]);
        let cwd = check.optional_string(&object, "cwd", &[]);
        let cwds = check.optional_strings(&object, "cwds", &[]);
        let _pinned = check.optional_bool(&object, "pinned", &[]);
        check.extras(
            &object,
            &[
                "cursor",
                "limit",
                "rootSessionId",
                "parentSessionId",
                "cwd",
                "cwds",
                "pinned",
            ],
            &[],
        );
        check.finish().map_err(rejected)?;
        let limit = usize::try_from(limit.unwrap_or(50)).unwrap_or(50);
        let store = self.store();
        store.migrate_legacy().map_err(storage_fault)?;
        // `cwds` is the union of what `cwd` matches for each entry, so an
        // explicit empty list matches nothing.
        let requested: Vec<Option<String>> = match cwds {
            Some(cwds) => cwds.into_iter().map(Some).collect(),
            None => vec![cwd],
        };
        let mut sessions: Vec<SessionInfo> = Vec::new();
        for cwd in &requested {
            for session in self.resume_sessions(cwd.as_deref())? {
                if !sessions
                    .iter()
                    .any(|known| known.session_id == session.session_id)
                {
                    sessions.push(session);
                }
            }
        }
        let roots = session_roots(&sessions);
        let mut filtered: Vec<SessionInfo> = sessions
            .into_iter()
            .filter(|session| {
                root_session_id
                    .as_ref()
                    .is_none_or(|root| roots.get(&session.session_id) == Some(root))
                    && parent_session_id
                        .as_ref()
                        .is_none_or(|parent| session.parent_session_id.as_ref() == Some(parent))
            })
            .collect();
        sort_sessions(&mut filtered);
        let start = session_cursor_index(&filtered, cursor.as_deref());
        let page: Vec<&SessionInfo> = filtered.iter().skip(start).take(limit).collect();
        let next_cursor = (start + page.len() < filtered.len())
            .then(|| page.last().map(|session| encode_session_cursor(session)))
            .flatten();
        let items: Vec<Value> = page
            .iter()
            .map(|session| {
                let root = roots
                    .get(&session.session_id)
                    .cloned()
                    .unwrap_or_else(|| session.session_id.clone());
                listed_session(&store, session, &root)
            })
            .collect();
        let continue_session_id = self.continue_session_id(&filtered);
        Ok(success_batch(
            request.id.clone(),
            result_map([
                ("items", json!(items)),
                ("nextCursor", json!(next_cursor)),
                ("previousCursor", Value::Null),
                ("continueSessionId", json!(continue_session_id)),
            ]),
        ))
    }

    /// Reference `_continue_resume_sessions`: the sessions begun in or moved
    /// to `cwd`, then the ones whose directory resolves to it, or maps onto it
    /// from a retained worktree.
    fn resume_sessions(&self, cwd: Option<&str>) -> Result<Vec<SessionInfo>, ProtocolFault> {
        let store = self.store();
        let mut sessions = store.sessions(cwd).map_err(storage_fault)?;
        let Some(cwd) = cwd else {
            return Ok(sessions);
        };
        let requested = resolve(cwd);
        let managed = self.server.workspace.managed_worktrees();
        let listed: BTreeSet<String> = sessions
            .iter()
            .map(|session| session.session_id.clone())
            .collect();
        for session in store.sessions(None).map_err(storage_fault)? {
            if listed.contains(&session.session_id) {
                continue;
            }
            let session_cwd = resolve(&session.cwd);
            let matches = session_cwd == requested
                || ManagedWorktree::at(&managed, &session_cwd)
                    .and_then(|held| held.retained_repository_mapping(&session_cwd))
                    .is_some_and(|mapping| {
                        requested.starts_with(&mapping.root) && mapping.cwd.starts_with(&requested)
                    });
            if matches {
                sessions.push(session);
            }
        }
        sort_sessions(&mut sessions);
        Ok(sessions)
    }

    /// Reference `_continue_session_id`: the terminal's last session when the
    /// page offers it, and otherwise the most recent one.
    fn continue_session_id(&self, filtered: &[SessionInfo]) -> Option<String> {
        let first = filtered.first()?;
        let pointer = self
            .server
            .workspace
            .session_logging()
            .enabled
            .then(|| self.store().pointer())
            .flatten();
        Some(
            pointer
                .filter(|pointer| {
                    filtered
                        .iter()
                        .any(|session| &session.session_id == pointer)
                })
                .unwrap_or_else(|| first.session_id.clone()),
        )
    }

    // ----------------------------------------------------------- reading

    /// Reference `session/read`: the live session answers for itself, and the
    /// store answers for any other.
    fn read_session(&self, request: &ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let object = object_of(&request.params);
        let mut check = WireCheck::default();
        let session_id = check.required_string(&object, "sessionId", &[]);
        let history_limit = check.bounded_int(&object, "historyLimit", &[], 200, PAGE_BOUNDS);
        let _turns_limit = check.bounded_int(&object, "turnsLimit", &[], 200, PAGE_BOUNDS);
        let _include_history = check.optional_bool(&object, "includeHistory", &[]);
        let _include_turns = check.optional_bool(&object, "includeTurns", &[]);
        check.extras(
            &object,
            &[
                "sessionId",
                "historyLimit",
                "turnsLimit",
                "includeHistory",
                "includeTurns",
            ],
            &[],
        );
        check.finish().map_err(rejected)?;
        let session_id = session_id.unwrap_or_default();
        let history_limit = usize::try_from(history_limit.unwrap_or(200)).unwrap_or(200);
        if let Some(state) = self.live_state(&session_id)? {
            let event_id = state["eventId"].clone();
            return Ok(success_batch(
                request.id.clone(),
                result_map([("state", state), ("lastEventId", event_id)]),
            ));
        }
        let hydrated = self.load_saved(&session_id)?;
        let state = stored_state(&session_id, &hydrated, history_limit);
        Ok(success_batch(
            request.id.clone(),
            result_map([("state", state), ("lastEventId", json!(0))]),
        ))
    }

    /// The state of a session this connection holds, or `None` for one it
    /// does not.
    fn live_state(&self, session_id: &str) -> Result<Option<Value>, ProtocolFault> {
        let sessions = self.server.lock_sessions()?;
        Ok(sessions
            .get(session_id)
            .filter(|session| {
                session.id == session_id
                    && session.status != SessionStatus::Closed
                    && sessions
                        .key(session_id)
                        .is_some_and(|key| self.attached_sessions.contains(key))
            })
            .map(public_session_state))
    }

    /// The saved session `session_id` names, as the host reads it
    /// (reference `HostRequestHandler._load_session`).
    fn load_saved(&self, session_id: &str) -> Result<HydratedSession, ProtocolFault> {
        match self.store().load(session_id) {
            Ok(hydrated) => Ok(hydrated),
            Err(StorageError::SessionNotFound(_) | StorageError::InvalidSessionId(_)) => {
                Err(not_found(session_id))
            }
            Err(error) => Err(storage_fault(error)),
        }
    }

    /// Reference `_get_session_history`: always the saved transcript, the
    /// latest `historyLimit` entries of it.
    fn history_get(&self, request: &ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let object = object_of(&request.params);
        let mut check = WireCheck::default();
        let session_id = check.required_string(&object, "sessionId", &[]);
        let limit = check.bounded_int(&object, "historyLimit", &[], 200, PAGE_BOUNDS);
        check.extras(&object, &["sessionId", "historyLimit"], &[]);
        check.finish().map_err(rejected)?;
        let session_id = session_id.unwrap_or_default();
        let limit = usize::try_from(limit.unwrap_or(200)).unwrap_or(200);
        let hydrated = self.load_saved(&session_id)?;
        let history = stored_history(&session_id, &hydrated);
        let tail = history.len().saturating_sub(limit);
        Ok(success_batch(
            request.id.clone(),
            result_map([("history", json!(history.get(tail..).unwrap_or_default()))]),
        ))
    }

    /// Reference `_history_list`: the live history for the root, the saved
    /// one for any other session.
    fn history_list(&self, request: &ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let object = object_of(&request.params);
        let mut check = WireCheck::default();
        let session_id = check.required_string(&object, "sessionId", &[]);
        let turn_id = check.optional_string(&object, "turnId", &[]);
        let page = page_request(&mut check, &object);
        check.extras(&object, &["sessionId", "turnId", "page"], &[]);
        check.finish().map_err(rejected)?;
        let session_id = session_id.unwrap_or_default();
        let page = page.unwrap_or_default();
        let live = (self.root_id().as_deref() == Some(session_id.as_str()))
            .then(|| {
                self.server.lock_sessions().ok().and_then(|sessions| {
                    sessions.get(&session_id).map(|session| {
                        session
                            .snapshot
                            .as_ref()
                            .map(|snapshot| snapshot.history.clone())
                            .unwrap_or_default()
                    })
                })
            })
            .flatten();
        let history = match live {
            Some(history) => history,
            None => stored_history(&session_id, &self.load_saved(&session_id)?),
        };
        let entries: Vec<&PublicHistoryEntry> = history
            .iter()
            .filter(|entry| {
                turn_id
                    .as_ref()
                    .is_none_or(|turn_id| entry.metadata().turn_id.as_ref() == Some(turn_id))
            })
            .collect();
        let (items, next, previous) =
            history_window(&entries, &page, |entry| entry.metadata().id.clone());
        Ok(success_batch(
            request.id.clone(),
            result_map([
                ("items", json!(items)),
                ("nextCursor", json!(next)),
                ("previousCursor", json!(previous)),
            ]),
        ))
    }

    /// Reference `_session_turns_list`: only the root has turns to page.
    fn turns_list(&self, request: &ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let object = object_of(&request.params);
        let mut check = WireCheck::default();
        let session_id = check.required_string(&object, "sessionId", &[]);
        let page = page_request(&mut check, &object);
        check.extras(&object, &["sessionId", "page"], &[]);
        if self.root_key().is_none() {
            return Err(ProtocolFault::plain(ProtocolErrorCode::Conflict, NO_ROOT));
        }
        check.finish().map_err(rejected)?;
        let session_id = session_id.unwrap_or_default();
        let page = page.unwrap_or_default();
        self.require_root(&session_id)?;
        let turns = {
            let sessions = self.server.lock_sessions()?;
            sessions
                .get(&session_id)
                .map(|session| session.turns.clone())
                .unwrap_or_default()
        };
        let turns: Vec<&PublicTurn> = turns.iter().collect();
        let (items, next, previous) = turns_window(&turns, &page, |turn| turn.id.clone());
        Ok(success_batch(
            request.id.clone(),
            result_map([
                ("items", json!(items)),
                ("nextCursor", json!(next)),
                ("previousCursor", json!(previous)),
            ]),
        ))
    }

    // ------------------------------------------------------------ titles

    /// Reference `session/rename`: the live session renames itself, and the
    /// store renames any other (`update_saved_session_title`).
    fn rename_session(&self, request: &ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let object = object_of(&request.params);
        let mut check = WireCheck::default();
        let session_id = check.required_string(&object, "sessionId", &[]);
        let title = check.required_string(&object, "title", &[]);
        check.extras(&object, &["sessionId", "title"], &[]);
        check.finish().map_err(rejected)?;
        let (session_id, title) = (session_id.unwrap_or_default(), title.unwrap_or_default());
        let store = self.store();
        let live = self.root_id().as_deref() == Some(session_id.as_str());
        let metadata = if live {
            rename_live(&store, &session_id, &title)
        } else {
            store.update_title(&session_id, &title)
        }
        .map_err(|error| match error {
            StorageError::InvalidTitle => {
                ProtocolFault::plain(ProtocolErrorCode::InvalidParams, error.to_string())
            }
            StorageError::SessionNotFound(_) | StorageError::InvalidSessionId(_) => {
                not_found(&session_id)
            }
            error => storage_fault(error),
        })?;
        if live
            && let Ok(mut sessions) = self.server.lock_sessions()
            && let Some(session) = sessions.get_mut(&session_id)
        {
            if let Some(snapshot) = session.snapshot.as_mut() {
                snapshot.title.clone_from(&metadata.title);
            }
            if let Some(persisted) = session.persisted.as_mut() {
                persisted.metadata.title.clone_from(&metadata.title);
                persisted
                    .metadata
                    .title_source
                    .clone_from(&metadata.title_source);
            }
        }
        let updated_at = metadata
            .is_persisted(&store)
            .then(|| metadata.end_time.clone())
            .flatten();
        Ok(success_batch(
            request.id.clone(),
            result_map([
                ("title", json!(metadata.title)),
                ("updatedAt", json!(updated_at)),
                ("lastEventId", Value::Null),
            ]),
        ))
    }

    /// Reference `_session_log_read`: what the root session writes, and where.
    fn read_session_log(&self, request: &ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let object = object_of(&request.params);
        let mut check = WireCheck::default();
        let session_id = check.required_string(&object, "sessionId", &[]);
        check.extras(&object, &["sessionId"], &[]);
        if self.root_key().is_none() {
            return Err(ProtocolFault::plain(ProtocolErrorCode::Conflict, NO_ROOT));
        }
        check.finish().map_err(rejected)?;
        let session_id = self.require_root(&session_id.unwrap_or_default())?;
        Ok(success_batch(
            request.id.clone(),
            result_map([("log", self.server.session_log_summary(&session_id))]),
        ))
    }

    // ------------------------------------------------------------ moving

    /// Reference `session/relocate`: the root moves itself, and the store
    /// moves a session nobody holds (`_relocate_session`).
    fn relocate_session(&self, request: &ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let object = object_of(&request.params);
        let mut check = WireCheck::default();
        let session_id = check.required_string(&object, "sessionId", &[]);
        let cwd = check.required_string(&object, "cwd", &[]);
        check.extras(&object, &["sessionId", "cwd"], &[]);
        check.finish().map_err(rejected)?;
        let (session_id, cwd) = (session_id.unwrap_or_default(), cwd.unwrap_or_default());
        if self.root_key().is_some() {
            return self.relocate_root(request, &session_id, &cwd);
        }
        let hydrated = self.load_saved(&session_id)?;
        let current = hydrated
            .metadata
            .environment
            .get("working_directory")
            .cloned()
            .flatten()
            .filter(|current| !current.is_empty())
            .ok_or_else(|| {
                ProtocolFault::plain(
                    ProtocolErrorCode::InvalidParams,
                    format!("Session has no working directory: {session_id}"),
                )
            })?;
        let target = relocation_target(&self.server.workspace, &current, &cwd)?;
        self.store()
            .relocate(&hydrated.metadata.id, &target)
            .map_err(storage_fault)?;
        let hydrated = self.load_saved(&session_id)?;
        Ok(success_batch(
            request.id.clone(),
            result_map([("state", stored_state(&session_id, &hydrated, 200))]),
        ))
    }

    /// Reference `_relocate`: the live session moves, and its history closes
    /// on the checkpoint that records where it went.
    fn relocate_root(
        &self,
        request: &ServerRequest,
        session_id: &str,
        cwd: &str,
    ) -> Result<DispatchBatch, ProtocolFault> {
        self.require_root(session_id)?;
        let previous = {
            let sessions = self.server.lock_sessions()?;
            let session = sessions
                .get(session_id)
                .ok_or_else(|| not_found(session_id))?;
            if let Some(turn_id) = &session.active_turn {
                return Err(ProtocolFault::plain(
                    ProtocolErrorCode::Conflict,
                    format!("Session is busy running turn {turn_id}"),
                ));
            }
            session.working_directory.clone()
        };
        let target = relocation_target(&self.server.workspace, &previous, cwd)?;
        let store = self.store();
        let mut metadata = store.open(session_id).map_err(storage_fault)?.metadata;
        let mut sessions = self.server.lock_sessions()?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| not_found(session_id))?;
        if target != previous {
            vibe_core::storage::relocate_metadata(&mut metadata, &target);
            store.update_metadata(&metadata).map_err(storage_fault)?;
            if let Some(persisted) = session.persisted.as_mut() {
                persisted.metadata.environment = metadata.environment.clone();
                persisted
                    .metadata
                    .origin_directory
                    .clone_from(&metadata.origin_directory);
                persisted.metadata.working_directory.clone_from(&target);
            }
            session.working_directory.clone_from(&target);
            let checkpoint = checkpoint_entry(
                &session.id,
                "relocation",
                &format!("Moved to {target}"),
                json!({"cwd": target, "previousCwd": previous}),
            );
            if let Some(snapshot) = session.snapshot.as_mut() {
                snapshot.history.push(checkpoint);
            }
            session.turns.clear();
            session.latest_turn = None;
            session.updated_at = now_millis();
        }
        let state = public_session_state(session);
        drop(sessions);
        let mut batch = success_batch(request.id.clone(), result_map([("state", state)]));
        batch
            .outbound
            .extend(self.runtime_updated_frame(session_id));
        Ok(batch)
    }

    // --------------------------------------------------------- reopening

    /// Reference `session/resume` and `session/continue`: the saved session
    /// is reopened as the root, and answers with its state before its runtime
    /// is published.
    fn reopen_session(
        &mut self,
        request: ServerRequest,
        reopen: Reopen,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let object = object_of(&request.params);
        let mut check = WireCheck::default();
        let session_id = match reopen {
            Reopen::Resume => check.required_string(&object, "sessionId", &[]),
            Reopen::Continue => None,
        };
        let agent_config = check
            .optional_object(&object, "agentConfig", &[])
            .cloned()
            .unwrap_or_default();
        let history_limit = check.bounded_int(&object, "historyLimit", &[], 200, PAGE_BOUNDS);
        let declared: &[&str] = match reopen {
            Reopen::Resume => &["sessionId", "agentConfig", "historyLimit"],
            Reopen::Continue => &["agentConfig", "historyLimit"],
        };
        check.extras(&object, declared, &[]);
        check.finish().map_err(rejected)?;
        let history_limit = history_limit.unwrap_or(200);
        let config_cwd = ["cwd", "workdir"]
            .into_iter()
            .find_map(|key| agent_config.get(key).and_then(Value::as_str))
            .map(ToOwned::to_owned);
        let target = match reopen {
            Reopen::Resume => {
                let selector = session_id.unwrap_or_default();
                self.load_saved(&selector)?.metadata.id
            }
            Reopen::Continue => {
                if self.root_key().is_some() {
                    return Err(ProtocolFault::plain(
                        ProtocolErrorCode::Conflict,
                        "A session is already attached",
                    ));
                }
                // Reference `legacy_open_target`: the session a listing of
                // `cwd` offers to continue, else the latest one saved there.
                let listed = self.resume_sessions(config_cwd.as_deref())?;
                match self.continue_session_id(&listed) {
                    Some(session_id) => session_id,
                    None => self.continue_target(config_cwd.as_deref())?,
                }
            }
        };
        if self.root_id().as_deref() == Some(target.as_str()) {
            let state = self.live_state(&target)?.unwrap_or(Value::Null);
            let event_id = state["eventId"].clone();
            let mut batch = success_batch(
                request.id,
                result_map([("state", state), ("lastEventId", event_id)]),
            );
            batch.outbound.extend(self.runtime_updated_frame(&target));
            return Ok(batch);
        }
        self.server.acquire_lease(&target)?;
        let mut params = reopen_start_params(&agent_config);
        params.insert("resume".to_owned(), json!(target));
        params.insert("historyLimit".to_owned(), json!(history_limit));
        if !params.contains_key("agent") {
            // Reference `_AgentLoopBlueprint.agent_name`: a reopened session
            // runs under the agent the launch names, else the configured
            // default, whatever it was saved under.
            let agent = self.server.workspace.default_agent_name()?;
            params.insert("agent".to_owned(), json!(agent));
        }
        let start = ServerRequest { params, ..request };
        let opened = self.open_reopened(start);
        if opened.is_err() {
            self.server.release_lease(&target);
        }
        opened
    }

    /// Opens the session a reopening resolved, through the start path, and
    /// answers with the reference's reopening shape.
    fn open_reopened(&mut self, request: ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let id = request.id.clone();
        let mut params = from_params::<SessionStartParams>(&request.params)?;
        let mut resolution = self.resolve_worktree(&mut params)?;
        let opened = self.open_resolved_session(&params, &mut resolution);
        let (session_id, state, mcp_configs) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                self.undo_worktree(&resolution);
                return Err(error);
            }
        };
        let event_id = state["eventId"].clone();
        let mut batch = success_batch(
            id,
            result_map([("state", state), ("lastEventId", event_id)]),
        );
        batch
            .outbound
            .extend(self.runtime_updated_frame(&session_id));
        if !mcp_configs.is_empty() {
            batch.deferred.push(DeferredWork::ConfigureMcp {
                session_id,
                configs: mcp_configs,
            });
        }
        Ok(batch)
    }

    /// Reference `_find_session_to_continue`.
    fn continue_target(&self, cwd: Option<&str>) -> Result<String, ProtocolFault> {
        let logging = self.server.workspace.session_logging();
        if !logging.enabled {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::NotFound,
                "Session logging is disabled. Enable it in the configuration to continue or \
                 resume a session",
            ));
        }
        let cwd = cwd.map_or_else(
            || self.server.workspace.working_directory().to_path_buf(),
            PathBuf::from,
        );
        let cwd = fs::canonicalize(&cwd).unwrap_or(cwd);
        let cwd = cwd.to_string_lossy().into_owned();
        match self.store().continue_target(&cwd) {
            Ok(session_id) => Ok(session_id),
            Err(StorageError::NoSessions) => {
                let mut message = format!(
                    "No previous sessions found in {} for cwd={cwd}",
                    logging.save_dir.display()
                );
                let managed =
                    vibe_core::worktree::managed_worktrees_root(self.server.workspace.vibe_home());
                if Path::new(&cwd).starts_with(fs::canonicalize(&managed).unwrap_or(managed)) {
                    message.push_str(
                        ". This worktree has no session of its own yet: start one here, or \
                         name an existing session to resume",
                    );
                }
                Err(ProtocolFault::plain(ProtocolErrorCode::NotFound, message))
            }
            Err(error) => Err(storage_fault(error)),
        }
    }

    // ----------------------------------------------------------- forking

    /// Reference `_session_fork`: the root is copied onto a new session,
    /// through the turn `entryId` names, and the copy either replaces the root
    /// or is left saved for later.
    fn fork_session(&mut self, request: &ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let object = object_of(&request.params);
        let mut check = WireCheck::default();
        let _idempotency_key = check.optional_string(&object, "idempotencyKey", &[]);
        let source = check.required_string(&object, "sourceSessionId", &[]);
        let entry_id = check.optional_string(&object, "entryId", &[]);
        let _agent_config = check.optional_object(&object, "agentConfig", &[]);
        let _after_turn_id = check.optional_string(&object, "afterTurnId", &[]);
        let history_limit = check.bounded_int(&object, "historyLimit", &[], 200, PAGE_BOUNDS);
        let attach = check.optional_bool(&object, "attach", &[]).unwrap_or(true);
        check.extras(
            &object,
            &[
                "idempotencyKey",
                "sourceSessionId",
                "entryId",
                "agentConfig",
                "afterTurnId",
                "historyLimit",
                "attach",
            ],
            &[],
        );
        check.finish().map_err(rejected)?;
        let source = source.unwrap_or_default();
        let history_limit = usize::try_from(history_limit.unwrap_or(200)).unwrap_or(200);
        if self.root_id().as_deref() != Some(source.as_str()) {
            return Err(not_found(&source));
        }
        let (busy, logging_enabled) = {
            let sessions = self.server.lock_sessions()?;
            let session = sessions.get(&source).ok_or_else(|| not_found(&source))?;
            (
                session.active_turn.clone(),
                self.server.workspace.session_logging().enabled,
            )
        };
        if let Some(turn_id) = busy {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                format!("Session is busy running turn {turn_id}"),
            ));
        }
        if !attach && !logging_enabled {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                "Detached forks require session logging to be enabled",
            ));
        }
        let workspace = &self.server.workspace;
        let source_session = match workspace.publish_draft(&source)? {
            Some(published) => published,
            None => workspace.load_session(&source)?,
        };
        let keep = match &entry_id {
            Some(entry_id) => Some(fork_keep(&source_session.messages, entry_id).ok_or_else(
                || {
                    ProtocolFault::plain(
                        ProtocolErrorCode::NotFound,
                        format!("Forkable history entry not found: {entry_id}"),
                    )
                },
            )?),
            None => None,
        };
        let new_id = vibe_core::session_id::rotate_session_id(&source);
        self.server.acquire_lease(&new_id)?;
        let forked = workspace.fork_saved_session(&source, &new_id, keep);
        let forked = match forked {
            Ok(forked) => forked,
            Err(error) => {
                self.server.release_lease(&new_id);
                return Err(error.into());
            }
        };
        if !attach {
            self.server.release_lease(&new_id);
            let state = detached_fork_state(&self.server, &forked, history_limit);
            return Ok(success_batch(
                request.id.clone(),
                result_map([
                    ("sourceSessionId", json!(source)),
                    ("state", state),
                    ("lastEventId", json!(0)),
                ]),
            ));
        }
        let attachment = crate::workspace::runtime_attachment(&forked);
        self.server.attach_workspace_runtime(&attachment, None)?;
        if let Ok(mut sessions) = self.server.lock_sessions()
            && let Some(session) = sessions.get_mut(&new_id)
        {
            session.snapshot = Some(persisted_projection(
                &forked,
                u16::try_from(history_limit).unwrap_or(u16::MAX),
                &session.working_directory,
            ));
        }
        self.retire_root();
        self.attached_sessions.insert(new_id.clone());
        self.root = Some(new_id.clone());
        let state = self.live_state(&new_id)?.unwrap_or(Value::Null);
        Ok(success_batch(
            request.id.clone(),
            result_map([
                ("sourceSessionId", json!(source)),
                ("state", state),
                ("lastEventId", json!(0)),
            ]),
        ))
    }

    /// Lets the root go: the connection detaches from it and its lease is
    /// released, which is what adopting a replacement does upstream.
    fn retire_root(&mut self) {
        let Some(root) = self.root.take() else {
            return;
        };
        let current = self.root_id_of(&root);
        if let Ok(mut sessions) = self.server.lock_sessions()
            && let Some(session) = sessions.get_mut(&root)
            && self.attached_sessions.remove(&root)
        {
            session.attachments = session.attachments.saturating_sub(1);
        }
        self.server
            .release_lease(current.as_deref().unwrap_or(&root));
    }

    fn root_id_of(&self, key: &str) -> Option<String> {
        let sessions = self.server.lock_sessions().ok()?;
        sessions.get(key).map(|session| session.id.clone())
    }

    // ---------------------------------------------------------- clearing

    /// Reference `_history_clear`: the root continues under a new identifier
    /// with nothing said yet, and the session it leaves stays saved.
    fn clear_history(&mut self, request: &ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let object = object_of(&request.params);
        let mut check = WireCheck::default();
        let session_id = check.required_string(&object, "sessionId", &[]);
        check.extras(&object, &["sessionId"], &[]);
        if self.root_key().is_none() {
            return Err(ProtocolFault::plain(ProtocolErrorCode::Conflict, NO_ROOT));
        }
        check.finish().map_err(rejected)?;
        let session_id = self.require_root(&session_id.unwrap_or_default())?;
        let key = self.root_key().unwrap_or_else(|| session_id.clone());
        {
            let sessions = self.server.lock_sessions()?;
            if sessions
                .get(&key)
                .is_some_and(|session| session.active_turn.is_some())
            {
                return Err(ProtocolFault::plain(
                    ProtocolErrorCode::Conflict,
                    "Cannot clear history while a turn is active",
                ));
            }
        }
        let workspace = &self.server.workspace;
        let source = match workspace.publish_draft(&session_id)? {
            Some(published) => published,
            None => workspace.load_session(&session_id)?,
        };
        let draft = workspace.clear_session(&source)?;
        let new_id = draft.metadata.id.clone();
        self.server.transfer_lease(&session_id, &new_id)?;
        self.server
            .projects
            .rebind_session(&session_id, &new_id)
            .map_err(|error| ServerError::Projects(error.to_string()))?;
        let now = now_millis();
        let mut sessions = self.server.lock_sessions()?;
        sessions.rename(&key, &new_id)?;
        let session = sessions
            .get_mut(&new_id)
            .ok_or_else(|| not_found(&new_id))?;
        session.intent.resume = Some(new_id.clone());
        session.persisted = Some(draft);
        session.created_at = now;
        session.updated_at = now;
        session.bumped_at = None;
        session.event_watermark = 0;
        session.stats = crate::server::runtime::SessionStats::default();
        let mut kept = session
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.history.clone())
            .unwrap_or_default();
        for entry in &mut kept {
            entry.rebind_session(new_id.clone());
        }
        kept.push(checkpoint_entry(
            &new_id,
            "clear",
            "New conversation started",
            Value::Null,
        ));
        let snapshot = session.snapshot.get_or_insert_with(|| ProjectionSnapshot {
            session_id: new_id.clone(),
            turn_id: None,
            handoff_cause: None,
            watermark: 0,
            lifecycle: LifecycleState::Idle,
            title: None,
            history: Vec::new(),
        });
        snapshot.session_id.clone_from(&new_id);
        snapshot.title = None;
        snapshot.watermark = 0;
        snapshot.turn_id = None;
        snapshot.lifecycle = LifecycleState::Idle;
        snapshot.history = kept;
        session.turns.clear();
        session.latest_turn = None;
        session.status = SessionStatus::Idle;
        let state = public_session_state(session);
        drop(sessions);
        super::session_management::reset_checkpoint_log(self, &new_id, 1)?;
        Ok(success_batch(
            request.id.clone(),
            result_map([
                ("state", state),
                ("sessionLog", self.server.session_log_summary(&new_id)),
            ]),
        ))
    }

    // ---------------------------------------------------------- deleting

    /// Reference `_delete_session`: the root cannot be deleted from under
    /// itself, and any other session is removed with whatever links to it.
    fn delete_saved_session(
        &self,
        request: &ServerRequest,
    ) -> Result<DispatchBatch, ProtocolFault> {
        let object = object_of(&request.params);
        let mut check = WireCheck::default();
        let session_id = check.required_string(&object, "sessionId", &[]);
        check.extras(&object, &["sessionId"], &[]);
        check.finish().map_err(rejected)?;
        let session_id = session_id.unwrap_or_default();
        let held = self.root_id().as_deref() == Some(session_id.as_str())
            || self.server.lock_sessions().is_ok_and(|sessions| {
                sessions.get(&session_id).is_some_and(|session| {
                    session.id == session_id
                        && session.attachments > 0
                        && session.status != SessionStatus::Closed
                })
            });
        if held {
            return Err(ProtocolFault::plain(
                ProtocolErrorCode::Conflict,
                "Deleting a live session is not supported",
            ));
        }
        let store = self.store();
        crate::session_lifecycle::delete_session_transactionally(
            &self.server.projects,
            &session_id,
            || {
                store
                    .delete(&session_id)
                    .map(|_| ())
                    .or_else(|error| match error {
                        StorageError::SessionNotFound(_) | StorageError::InvalidSessionId(_) => {
                            Ok(())
                        }
                        error => Err(error),
                    })
            },
        )
        .map_err(|error| match error {
            crate::session_lifecycle::DeleteSessionError::Prepare(error) => {
                ProtocolFault::from(error)
            }
            crate::session_lifecycle::DeleteSessionError::Delete(error) => storage_fault(error),
            crate::session_lifecycle::DeleteSessionError::Rollback { delete, rollback } => {
                ProtocolFault::internal(format!(
                    "session delete failed ({delete}); rollback failed ({rollback})"
                ))
            }
        })?;
        Ok(success_batch(request.id.clone(), BTreeMap::new()))
    }

    /// `runtime/updated` for `session_id`, which a reopening publishes once
    /// its answer is on the wire.
    pub(super) fn runtime_updated_frame(&self, session_id: &str) -> Option<Vec<u8>> {
        let runtime = self.server.runtime_snapshot(session_id)?;
        Some(encode_notification(
            "runtime/updated",
            result_map([("sessionId", json!(session_id)), ("runtime", runtime)]),
        ))
    }
}

/// Which saved session a reopening names.
#[derive(Clone, Copy)]
enum Reopen {
    Resume,
    Continue,
}

/// Reference `PageRequest`.
#[derive(Default)]
struct Page {
    cursor: Option<String>,
    limit: Option<usize>,
    forward: bool,
}

fn page_request(check: &mut WireCheck, object: &Map<String, Value>) -> Option<Page> {
    let parent = [PathSegment::Field("page".to_owned())];
    let page = check.optional_object(object, "page", &[])?;
    let cursor = check.optional_string(page, "cursor", &parent);
    let limit = check.bounded_int(page, "limit", &parent, 200, PAGE_BOUNDS);
    let forward = match page.get("direction") {
        None | Some(Value::Null) => false,
        Some(Value::String(direction)) if direction == "forward" => true,
        Some(Value::String(direction)) if direction == "backward" => false,
        Some(_) => {
            check.report(
                crate::params::child_path(&parent, "direction"),
                "Input should be 'forward' or 'backward'",
            );
            false
        }
    };
    check.extras(page, &["cursor", "limit", "direction"], &parent);
    Some(Page {
        cursor,
        limit: limit.and_then(|limit| usize::try_from(limit).ok()),
        forward,
    })
}

/// Reference `history_page` as `_history_list` reads it: the latest entries
/// unless a cursor names where to page from, the cursor read as the entry to
/// stop before going backward and to start after going forward.
fn history_window<'a, T>(
    items: &[&'a T],
    page: &Page,
    id: impl Fn(&T) -> String,
) -> (Vec<&'a T>, Option<String>, Option<String>) {
    let limit = page.limit.unwrap_or(200);
    let position = |cursor: &str| items.iter().position(|item| id(item) == cursor);
    let (first, end) = match page.cursor.as_deref() {
        Some(cursor) if page.forward => {
            let first = position(cursor).map_or(items.len(), |index| index + 1);
            (first, (first + limit).min(items.len()))
        }
        Some(cursor) => {
            let end = position(cursor).unwrap_or(0);
            (end.saturating_sub(limit), end)
        }
        None => (items.len().saturating_sub(limit), items.len()),
    };
    let window: Vec<&'a T> = items.get(first..end).unwrap_or_default().to_vec();
    let before = (first > 0)
        .then(|| window.first().map(|item| id(item)))
        .flatten();
    let after = (end < items.len())
        .then(|| window.last().map(|item| id(item)))
        .flatten();
    if page.forward {
        (window, after, before)
    } else {
        (window, before, after)
    }
}

/// Reference `_session_turns_list`: going backward, the latest turns or the
/// ones before the cursor; going forward, the first turns or the ones after
/// it.
fn turns_window<'a, T>(
    items: &[&'a T],
    page: &Page,
    id: impl Fn(&T) -> String,
) -> (Vec<&'a T>, Option<String>, Option<String>) {
    let limit = page.limit.unwrap_or(200);
    let position = |cursor: &str| items.iter().position(|item| id(item) == cursor);
    let (first, end) = if page.forward {
        let first = page.cursor.as_deref().map_or(0, |cursor| {
            position(cursor).map_or(items.len(), |index| index + 1)
        });
        (first, (first + limit).min(items.len()))
    } else {
        match page.cursor.as_deref() {
            None => (items.len().saturating_sub(limit), items.len()),
            Some(cursor) => {
                let end = position(cursor).unwrap_or(0);
                (end.saturating_sub(limit), end)
            }
        }
    };
    let window: Vec<&'a T> = items.get(first..end).unwrap_or_default().to_vec();
    let next = (first > 0)
        .then(|| window.first().map(|item| id(item)))
        .flatten();
    let previous = (end < items.len())
        .then(|| window.last().map(|item| id(item)))
        .flatten();
    if page.forward {
        (window, previous, next)
    } else {
        (window, next, previous)
    }
}

/// The start parameters a reopening's `agentConfig` spells, in the names the
/// start path reads.
fn reopen_start_params(agent_config: &Map<String, Value>) -> BTreeMap<String, Value> {
    const CARRIED: [&str; 12] = [
        "cwd",
        "workspaceRoots",
        "agent",
        "autoApprove",
        "enabledTools",
        "disabledTools",
        "maxTurns",
        "maxPrice",
        "maxSessionTokens",
        "headless",
        "trustWorkspace",
        "mcpServers",
    ];
    let mut params = BTreeMap::new();
    for key in CARRIED {
        if let Some(value) = agent_config.get(key).filter(|value| !value.is_null()) {
            params.insert(key.to_owned(), value.clone());
        }
    }
    if !params.contains_key("cwd")
        && let Some(workdir) = agent_config
            .get("workdir")
            .filter(|value| value.is_string())
    {
        params.insert("cwd".to_owned(), workdir.clone());
    }
    if let Some(worktree) = agent_config
        .get("worktree")
        .filter(|value| !value.is_null())
    {
        params.insert("worktree".to_owned(), worktree.clone());
    }
    params
}

/// How many stored messages a fork anchored at the user message `entry_id`
/// keeps: that message and the turn it opened (reference
/// `_messages_for_fork`). Only a user message the operator wrote anchors one.
fn fork_keep(messages: &[ModelMessage], entry_id: &str) -> Option<usize> {
    let anchor = crate::workspace::rewind_entry_index(messages, entry_id)?;
    Some(
        messages
            .iter()
            .enumerate()
            .skip(anchor + 1)
            .find(|(_, message)| {
                matches!(
                    message,
                    ModelMessage::User {
                        injected: false,
                        ..
                    }
                )
            })
            .map_or(messages.len(), |(index, _)| index),
    )
}

/// Reference `_relocation_target` and `_is_counterpart`: a directory, and one
/// of the checkouts of the repository the session already sits in.
fn relocation_target(
    workspace: &WorkspaceService,
    current: &str,
    requested: &str,
) -> Result<String, ProtocolFault> {
    let expanded = crate::host::expand_home(Path::new(requested));
    let target = fs::canonicalize(&expanded).unwrap_or_else(|_| absolute(&expanded));
    if !target.is_dir() {
        return Err(ProtocolFault::plain(
            ProtocolErrorCode::InvalidParams,
            format!("Not a directory: {}", target.display()),
        ));
    }
    let managed = workspace.managed_worktrees();
    let counterparts = WorktreeRepository::open(Path::new(current), &managed)
        .and_then(|repository| {
            let mut counterparts: Vec<PathBuf> = repository
                .linked()?
                .into_iter()
                .map(|worktree| fs::canonicalize(&worktree.path).unwrap_or(worktree.path))
                .collect();
            counterparts.extend(repository.repository_counterpart());
            Ok(counterparts)
        })
        .unwrap_or_default();
    if !counterparts.contains(&target) {
        return Err(ProtocolFault::plain(
            ProtocolErrorCode::InvalidParams,
            format!(
                "Not a worktree of this session's repository: {}",
                target.display()
            ),
        ));
    }
    Ok(target.to_string_lossy().into_owned())
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

/// `Path.expanduser().resolve()`, as far as the directory exists.
fn resolve(cwd: &str) -> PathBuf {
    let expanded = crate::host::expand_home(Path::new(cwd));
    fs::canonicalize(&expanded).unwrap_or_else(|_| absolute(&expanded))
}

/// Reference `_session_roots`: each session's furthest ancestor among the
/// listed ones, or itself.
fn session_roots(sessions: &[SessionInfo]) -> BTreeMap<String, String> {
    let parents: BTreeMap<&str, Option<&str>> = sessions
        .iter()
        .map(|session| {
            (
                session.session_id.as_str(),
                session.parent_session_id.as_deref(),
            )
        })
        .collect();
    let mut roots = BTreeMap::new();
    for session_id in parents.keys() {
        let mut seen = BTreeSet::new();
        let mut current = *session_id;
        while let Some(Some(parent)) = parents.get(current)
            && parents.contains_key(parent)
        {
            if !seen.insert(current) {
                current = session_id;
                break;
            }
            current = parent;
        }
        roots.insert((*session_id).to_owned(), current.to_owned());
    }
    roots
}

/// Most recently updated first, the identifier breaking a tie.
fn sort_sessions(sessions: &mut [SessionInfo]) {
    sessions.sort_by(|left, right| {
        (&right.updated_at, &right.session_id).cmp(&(&left.updated_at, &left.session_id))
    });
}

/// Reference `_encode_session_cursor`.
fn encode_session_cursor(session: &SessionInfo) -> String {
    URL_SAFE_NO_PAD.encode(format!("{}\0{}", session.updated_at, session.session_id))
}

/// Reference `_session_cursor_index`: past the session the cursor names, and
/// past everything when it names none.
fn session_cursor_index(sessions: &[SessionInfo], cursor: Option<&str>) -> usize {
    let Some(cursor) = cursor else {
        return 0;
    };
    let decoded = URL_SAFE_NO_PAD
        .decode(cursor.trim_end_matches('='))
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok());
    let Some((updated_at, session_id)) = decoded
        .as_deref()
        .and_then(|decoded| decoded.split_once('\0'))
    else {
        return sessions.len();
    };
    sessions
        .iter()
        .position(|session| session.updated_at == updated_at && session.session_id == session_id)
        .map_or(sessions.len(), |index| index + 1)
}

/// Reference `time_ms`.
fn time_ms(value: Option<&str>) -> u64 {
    value.and_then(parse_iso_millis).unwrap_or_else(now_millis)
}

/// One `PublicSession` of a listing, as the host projects it.
fn listed_session(store: &SessionStore, session: &SessionInfo, root: &str) -> Value {
    json!({
        "id": session.session_id,
        "rootSessionId": root,
        "parentSessionId": session.parent_session_id,
        "title": session.title,
        "preview": store.first_user_message(&session.session_id),
        "status": {"type": "idle"},
        "createdAt": time_ms(session.start_time.as_deref().or(Some(&session.updated_at))),
        "updatedAt": time_ms(Some(&session.updated_at)),
        "bumpedAt": session.bumped_at.as_deref().and_then(parse_iso_millis),
        "pinnedAt": null,
        "cwd": (!session.cwd.is_empty()).then_some(&session.cwd),
        "workspaceRoots": [],
        "model": null,
        "reasoningEffort": null,
        "agent": null,
        "tokenUsage": null,
        "contextUsage": null,
        "harness": null,
    })
}

/// Reference `project_message_history` over a saved transcript.
fn stored_history(session_id: &str, hydrated: &HydratedSession) -> Vec<PublicHistoryEntry> {
    let working_directory = hydrated.metadata.working_directory.clone();
    let mut history = persisted_projection(hydrated, u16::MAX, &working_directory).history;
    for entry in &mut history {
        entry.rebind_session(session_id.to_owned());
    }
    history
}

/// Reference `message_preview`: the first thing the operator typed, cut at
/// 160 characters.
fn message_preview(messages: &[ModelMessage]) -> String {
    messages
        .iter()
        .find_map(|message| match message {
            ModelMessage::User {
                content,
                injected: false,
                ..
            } if !content.is_empty() => Some(content.chars().take(160).collect()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Reference `build_stored_public_state`.
fn stored_state(session_id: &str, hydrated: &HydratedSession, history_limit: usize) -> Value {
    let metadata = &hydrated.metadata;
    let history = stored_history(session_id, hydrated);
    let retained_from = history.len().saturating_sub(history_limit);
    let before = (history.len() > history_limit)
        .then(|| {
            history
                .get(retained_from)
                .map(|entry| entry.metadata().id.clone())
        })
        .flatten();
    let cwd = metadata
        .environment
        .get("working_directory")
        .cloned()
        .flatten();
    json!({
        "format": "vibe.public-session-state/v1",
        "eventId": 0,
        "session": {
            "id": session_id,
            "rootSessionId": metadata.parent_session_id.as_deref().unwrap_or(session_id),
            "parentSessionId": metadata.parent_session_id,
            "title": metadata.title,
            "preview": message_preview(&hydrated.messages),
            "status": {"type": "idle"},
            "createdAt": time_ms(Some(&metadata.start_time)),
            "updatedAt": time_ms(metadata.end_time.as_deref()),
            "bumpedAt": metadata.bumped_at.as_deref().and_then(parse_iso_millis),
            "pinnedAt": null,
            "cwd": cwd,
            "workspaceRoots": [],
            "model": null,
            "reasoningEffort": null,
            "agent": null,
            "tokenUsage": null,
            "contextUsage": null,
            "harness": null,
        },
        "isQuiescent": null,
        "history": history.get(retained_from..).unwrap_or_default(),
        "historyBeforeCursor": before,
        "turns": [],
        "activeCallbacks": [],
        "childSessions": [],
        "turnQueue": {"items": [], "paused": false, "maxItems": 32},
        "retrying": null,
    })
}

/// Reference `build_public_state` over a fork nobody attached: the saved
/// transcript, under the runtime the fork was built with.
fn detached_fork_state(
    server: &AppServer,
    forked: &HydratedSession,
    history_limit: usize,
) -> Value {
    let mut state = stored_state(&forked.metadata.id, forked, history_limit);
    let agent = forked
        .metadata
        .agent_profile
        .as_ref()
        .and_then(|profile| serde_json::from_value::<AgentProfile>(profile.clone()).ok())
        .map(|profile| crate::workspace::agent_summary(&profile));
    let session = &mut state["session"];
    session["agent"] = json!(agent);
    session["model"] = json!(server.workspace.active_model_alias());
    session["workspaceRoots"] = json!([forked.metadata.working_directory]);
    session["tokenUsage"] = json!({"inputTokens": 0, "outputTokens": 0, "totalTokens": 0});
    state
}

/// Reference `AgentLoop.rename`: the root is renamed whether or not it was
/// written yet, and an unwritten one takes the title with its first save.
fn rename_live(
    store: &SessionStore,
    session_id: &str,
    title: &str,
) -> Result<vibe_core::storage::SessionMetadata, StorageError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(StorageError::InvalidTitle);
    }
    let mut metadata = store.open(session_id)?.metadata;
    metadata.title = Some(title.to_owned());
    "manual".clone_into(&mut metadata.title_source);
    store.update_metadata(&metadata)?;
    Ok(metadata)
}

fn not_found(session_id: &str) -> ProtocolFault {
    ProtocolFault::plain(
        ProtocolErrorCode::NotFound,
        format!("Session not found: {session_id}"),
    )
}

fn rejected(issues: Vec<InvalidParamsIssue>) -> ProtocolFault {
    ProtocolFault::InvalidParams(ParamsRejection::with_issues(issues))
}

fn storage_fault(error: StorageError) -> ProtocolFault {
    ProtocolFault::internal(error.to_string())
}

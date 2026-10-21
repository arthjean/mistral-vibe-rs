//! The methods that address saved sessions: listing and reading them,
//! reopening, forking, renaming, moving, clearing and deleting.
//!
//! The reference routes each one by whether the connection holds a root
//! session (`vibe/app_server/server.py` `_dispatch_to_root` and
//! `_dispatch_without_root`): the live session answers for itself, and the
//! store answers for every other one (`vibe/app_server/_host.py`). A method the
//! host does not serve needs a root, and without one the answer is a conflict.

use super::*;
use crate::wire_validation;
use serde_json::Map;
use vibe_core::storage::lease::{LeaseError, SessionLease};
use vibe_core::storage::{SessionInfo, SessionStore, StorageError};
use vibe_core::time::parse_iso_millis;

mod clearing;
mod deleting;
mod forking;
mod listing;
mod moving;
mod reading;
mod reopening;
mod titles;

/// Reference `_open_initial_session_locked`: what a method needing a root
/// answers without one.
pub(in crate::server) const NO_ROOT: &str =
    "Start, resume, or continue a session before using this method";

impl AppServer {
    /// Takes the lease on `session_id` for this process, or reports who holds
    /// it (reference `_acquire_session_lease`). Nothing is leased while session
    /// logging is off, and a session this process holds already stays held.
    pub(in crate::server) fn acquire_lease(&self, session_id: &str) -> Result<(), ProtocolFault> {
        if !self.workspace.session_logging().enabled {
            return Ok(());
        }
        let mut leases = lock(&self.leases);
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
    pub(in crate::server) fn release_lease(&self, session_id: &str) {
        let lease = lock(&self.leases).remove(session_id);
        drop(lease);
    }

    /// Moves the lease a session holds onto the identifier it continues under.
    pub(in crate::server) fn transfer_lease(
        &self,
        from: &str,
        to: &str,
    ) -> Result<(), ProtocolFault> {
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
            "session/fork" if self.server.harness_selection().use_unified => {
                self.unified_fork(&request)
            }
            "session/fork" => self.fork_session(&request),
            "session/delete" => self.delete_saved_session(&request),
            "session/history/clear" if self.server.harness_selection().use_unified => {
                self.unified_clear(&request)
            }
            "session/history/clear" => self.clear_history(&request),
            _ => Err(ProtocolFault::plain(
                ProtocolErrorCode::MethodNotFound,
                format!("Method not found: {}", request.method),
            )),
        };
        answered(id, outcome)
    }

    /// Reference `LegacySessionStore.list_sessions`: the sessions an older
    /// Vibe saved, as rows of a unified listing.
    pub(in crate::server) fn legacy_rows(&self, cwd: Option<&str>) -> Vec<Value> {
        let store = self.store();
        self.resume_sessions(cwd)
            .unwrap_or_default()
            .iter()
            .map(|session| {
                let mut row = listed_session(&store, session, "");
                row["rootSessionId"] = Value::Null;
                row["harness"] = json!("legacy");
                row["pinnedAt"] = json!(session.pinned_at.as_deref().and_then(parse_iso_millis));
                if session.title.is_some() {
                    row["preview"] = json!("");
                }
                row
            })
            .collect()
    }

    /// Reference `LegacySessionStore.read_session`: a legacy session not
    /// yet imported, previewed from its transcript.
    pub(in crate::server) fn legacy_state(
        &self,
        session_id: &str,
        history_limit: Option<usize>,
    ) -> Option<Value> {
        let hydrated = self.load_saved(session_id).ok()?;
        let id = hydrated.metadata.id.clone();
        let mut state = stored_state(&id, &hydrated, history_limit.unwrap_or(usize::MAX));
        let session = &mut state["session"];
        session["rootSessionId"] = Value::Null;
        session["harness"] = json!("legacy");
        session["updatedAt"] = session["createdAt"].clone();
        session["archivedAt"] = Value::Null;
        session["pinnedAt"] = json!(
            hydrated
                .metadata
                .pinned_at
                .as_deref()
                .and_then(parse_iso_millis)
        );
        if !session["title"].is_null() {
            session["preview"] = json!("");
        }
        Some(state)
    }

    /// The transcript and settings of the legacy session `session_id`.
    pub(in crate::server) fn legacy_session(&self, session_id: &str) -> Option<HydratedSession> {
        self.load_saved(session_id).ok()
    }

    /// The registry key of the session this connection is rooted in.
    pub(in crate::server) fn root_key(&self) -> Option<String> {
        let root = self.root.as_ref()?;
        let sessions = self.server.lock_sessions();
        sessions
            .get(root)
            .filter(|session| session.status != SessionStatus::Closed)
            .map(|_| root.clone())
    }

    /// The identifier the root session answers to now.
    pub(in crate::server) fn root_id(&self) -> Option<String> {
        let root = self.root_key()?;
        let sessions = self.server.lock_sessions();
        sessions.get(&root).map(|session| session.id.clone())
    }

    /// The identifier of the session this connection serves as its root,
    /// which a rewind into a fork or a cleared history changes.
    #[must_use]
    pub fn root_session_id(&self) -> Option<String> {
        self.root_id()
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

    /// The root's identifier, refused as a method addressed to the root is
    /// when the connection holds none.
    pub(in crate::server) fn root_id_required(&self) -> Result<String, ProtocolFault> {
        self.root_id()
            .ok_or_else(|| ProtocolFault::plain(ProtocolErrorCode::Conflict, NO_ROOT))
    }

    /// The root, which `session_id` has to name (reference
    /// `_require_session`). Without a root the method was never the host's.
    fn require_root(&self, session_id: &str) -> Result<String, ProtocolFault> {
        let root = self.root_id_required()?;
        if root == session_id {
            Ok(root)
        } else {
            Err(not_found(session_id))
        }
    }

    fn store(&self) -> SessionStore {
        self.server.workspace.session_store()
    }

    /// `runtime/updated` for `session_id`, which a reopening publishes once
    /// its answer is on the wire.
    pub(crate) fn runtime_updated_frame(&self, session_id: &str) -> Option<Vec<u8>> {
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

// Each handler reads its parameters past the schema that validated them
// (`wire_validation::validate_params`): only the fields it uses, each
// defaulted as the reference model defaults it.

/// The parameters of a method that names nothing but its session.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionParams {
    session_id: String,
}

/// The start parameters a reopening's `agentConfig` spells, in the names the
/// start path reads.
pub(in crate::server) fn reopen_start_params(
    agent_config: &Map<String, Value>,
) -> BTreeMap<String, Value> {
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
    vibe_core::platform::canonicalize(&expanded).unwrap_or_else(|_| absolute(&expanded))
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
        "archivedAt": null,
        "isUnseen": false,
        "cwd": (!session.cwd.is_empty()).then_some(&session.cwd),
        "workspaceRoots": [],
        "worktree": null,
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
            "archivedAt": metadata.archived_at.as_deref().and_then(parse_iso_millis),
            "isUnseen": false,
            "cwd": cwd,
            "workspaceRoots": [],
            "worktree": null,
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
        "backgroundProcesses": [],
        "turnQueue": {"items": [], "paused": false, "maxItems": 32},
        "retrying": null,
    })
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

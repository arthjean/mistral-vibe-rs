//! `session/rewind/read` and `session/rewind`: taking a conversation back to
//! one of the messages its operator wrote.
//!
//! Reference `_handler._rewind_read` and `_handler._rewind`
//! (`vibe/app_server/_handler.py`), over `RewindManager.rewind_to_message`
//! (`vibe/core/rewind/manager.py`). A rewind in place truncates the session it
//! names and writes it; a fork keeps that session whole and moves the
//! connection onto an unwritten successor, which its first turn writes.

use super::*;
use crate::workspace::{WorkspaceServiceError, reference_message_index, rewind_entry_index};
use vibe_core::events::ModelMessage;

/// The message a rewind goes back to.
struct RewindTarget {
    /// Its position in the stored transcript, which is where the transcript
    /// is cut.
    stored: usize,
    /// Its position in the reference's message list, which opens with the
    /// system prompt: the number the checkpoint log opened that turn under
    /// (`vibe/core/rewind/manager.py` addresses the log by it).
    turn: usize,
    /// What the operator wrote there.
    message: String,
}

/// Reference `SessionRewindParams`, which `SessionRewindReadParams` is the
/// first two fields of.
struct RewindParams {
    session_id: String,
    entry_id: String,
    restore_files: bool,
    inplace: bool,
}

impl ServerConnection {
    pub(super) fn session_rewind_read(&mut self, request: ServerRequest) -> DispatchBatch {
        let id = request.id.clone();
        answered(id, self.read_rewind(&request))
    }

    pub(super) fn session_rewind(&mut self, request: ServerRequest) -> DispatchBatch {
        let id = request.id.clone();
        answered(id, self.rewind(&request))
    }

    fn read_rewind(&self, request: &ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let params = rewind_params(&request.params, false)?;
        let (review, working_directory) = {
            let sessions = self.server.lock_sessions()?;
            let session = current_session(&sessions, &params.session_id)?;
            (session.review.clone(), session.working_directory.clone())
        };
        let target = self.rewind_target(&params)?;
        let paths = match review {
            Some(review) => review
                .restorable_paths_at(target.turn)
                .map_err(|error| ServerError::Resource(error.to_string()))?,
            None => Vec::new(),
        };
        let paths = absolute_paths(&working_directory, paths);
        Ok(success_batch(
            request.id.clone(),
            result_map([
                ("hasFileChanges", json!(!paths.is_empty())),
                ("paths", json!(paths)),
            ]),
        ))
    }

    fn rewind(&self, request: &ServerRequest) -> Result<DispatchBatch, ProtocolFault> {
        let params = rewind_params(&request.params, true)?;
        let mut sessions = self.server.lock_sessions()?;
        current_session(&sessions, &params.session_id)?;
        // The registry keeps a session under the identifier it opened with,
        // which is what the connection recorded when it attached.
        let key = sessions
            .key(&params.session_id)
            .map(ToOwned::to_owned)
            .ok_or_else(|| session_missing("Session not found"))?;
        if !self.attached_sessions.contains(&key) {
            return Err(ProtocolFault::new(
                ProtocolErrorCode::Conflict,
                "Session is not attached",
            ));
        }
        let RewindTarget {
            stored: index,
            turn,
            message,
        } = self.rewind_target(&params)?;
        let session = sessions
            .get_mut(&key)
            .ok_or_else(|| session_missing("Session not found"))?;
        // Reference `SessionExecution.reserve`: the entry is resolved before
        // the reservation, so a rewind to an unknown entry answers `not_found`
        // even while a turn runs.
        if let Some(turn_id) = &session.active_turn {
            return Err(ProtocolFault::new(
                ProtocolErrorCode::Conflict,
                format!("Session is busy running turn {turn_id}"),
            ));
        }
        if session.compaction_pending {
            return Err(ProtocolFault::new(
                ProtocolErrorCode::Conflict,
                "Session is busy running lifecycle compact",
            ));
        }
        let history = session
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.history.clone())
            .unwrap_or_default();
        let position = history
            .iter()
            .position(|entry| entry.metadata().id == params.entry_id);
        let (restore_errors, restored_paths) = match (&session.review, params.restore_files) {
            (Some(review), true) => {
                let staged = review
                    .stage_restore_to_message(turn)
                    .map_err(|error| ServerError::Resource(error.to_string()))?;
                (staged.errors, staged.transaction.commit())
            }
            _ => (Vec::new(), Vec::new()),
        };
        let restored_paths = absolute_paths(&session.working_directory, restored_paths);
        let workspace = &self.server.workspace;
        let now = now_millis();
        if params.inplace {
            session.persisted = Some(workspace.truncate_session(&params.session_id, index)?);
        } else {
            // Reference `_save_messages` before `_reset_session`: the session
            // being left is written whole, which only an unwritten fork is not.
            let source = match workspace.publish_draft(&params.session_id)? {
                Some(published) => published,
                None => workspace.load_session(&params.session_id)?,
            };
            let draft = workspace.fork_draft(&source, index)?;
            let new_id = draft.metadata.id.clone();
            self.server
                .projects
                .rebind_session(&params.session_id, &new_id)
                .map_err(|error| ServerError::Projects(error.to_string()))?;
            sessions.rename(&key, &new_id)?;
            let session = sessions
                .get_mut(&new_id)
                .ok_or_else(|| session_missing("Session not found"))?;
            session.intent.resume = Some(new_id);
            session.persisted = Some(draft);
            session.created_at = now;
            session.bumped_at = None;
            session.event_watermark = 0;
            if let Some(snapshot) = session.snapshot.as_mut() {
                snapshot.title = None;
                snapshot.watermark = 0;
            }
        }
        let session = sessions
            .get_mut(&params.session_id)
            .ok_or_else(|| session_missing("Session not found"))?;
        if let Some(review) = &session.review {
            review
                .drop_turns_from(turn)
                .map_err(|error| ServerError::Resource(error.to_string()))?;
        }
        // Reference `replace_idle_with_history`: the entries before the one
        // rewound to, rebound to the session that now holds them, then the
        // checkpoint that marks the rewind. A resumed session holds only the
        // latest page of its history live, so an entry older than that is
        // kept from the transcript the rewind left instead.
        let session_id = session.id.clone();
        let mut kept = match (position, &session.persisted) {
            (Some(position), _) => {
                let mut kept = history;
                kept.truncate(position);
                kept
            }
            (None, Some(persisted)) => {
                persisted_projection(persisted, u16::MAX, &session.working_directory).history
            }
            (None, None) => Vec::new(),
        };
        for entry in &mut kept {
            entry.rebind_session(session_id.clone());
        }
        kept.push(checkpoint_entry(
            &session_id,
            "rewind",
            "Conversation rewound",
            json!({
                "entryId": params.entry_id,
                "restoreFiles": params.restore_files,
                "inplace": params.inplace,
            }),
        ));
        let snapshot = session.snapshot.get_or_insert_with(|| ProjectionSnapshot {
            session_id: session_id.clone(),
            turn_id: None,
            handoff_cause: None,
            watermark: 0,
            lifecycle: LifecycleState::Idle,
            title: None,
            history: Vec::new(),
        });
        snapshot.session_id.clone_from(&session_id);
        snapshot.turn_id = None;
        snapshot.lifecycle = LifecycleState::Idle;
        snapshot.history = kept;
        session.turns.clear();
        session.latest_turn = None;
        session.status = SessionStatus::Idle;
        session.updated_at = now;
        let state = public_session_state(session);
        drop(sessions);
        Ok(success_batch(
            request.id.clone(),
            result_map([
                ("message", json!(message)),
                ("restoreErrors", json!(restore_errors)),
                ("restoredPaths", json!(restored_paths)),
                ("state", state),
                ("sessionLog", self.server.session_log_summary(&session_id)),
            ]),
        ))
    }

    /// The message `params` names and what the operator wrote there.
    ///
    /// Reference `history_user_message_index`: a session nothing was written
    /// to yet has no rewindable entry, which is the same refusal as an entry
    /// it does not hold.
    fn rewind_target(&self, params: &RewindParams) -> Result<RewindTarget, ProtocolFault> {
        let messages = match self.server.workspace.load_session(&params.session_id) {
            Ok(hydrated) => hydrated.messages,
            Err(WorkspaceServiceError::NotFound(_)) => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        rewind_entry_index(&messages, &params.entry_id)
            .and_then(|index| match messages.get(index) {
                Some(ModelMessage::User { content, .. }) => Some(RewindTarget {
                    stored: index,
                    turn: reference_message_index(&messages, index),
                    message: content.clone(),
                }),
                _ => None,
            })
            .ok_or_else(|| {
                ProtocolFault::new(
                    ProtocolErrorCode::NotFound,
                    format!("Rewindable history entry not found: {}", params.entry_id),
                )
            })
    }
}

/// The session `session_id` names, only under its current identifier.
///
/// Reference `RootSession.is_current`: an identifier a fork left behind no
/// longer names the session, even though other methods still resolve it.
fn current_session<'a>(
    sessions: &'a SessionRegistry,
    session_id: &str,
) -> Result<&'a SessionRuntime, ProtocolFault> {
    sessions
        .get(session_id)
        .filter(|session| session.id == session_id)
        .ok_or_else(|| {
            ProtocolFault::new(
                ProtocolErrorCode::NotFound,
                format!("Session not found: {session_id}"),
            )
        })
}

/// The paths the checkpoint log names, resolved against the session's
/// directory as the reference reports them.
fn absolute_paths(working_directory: &str, paths: Vec<String>) -> Vec<String> {
    paths
        .into_iter()
        .map(|path| {
            Path::new(working_directory)
                .join(&path)
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// Reads the parameters as the reference's pydantic model does: every field
/// under its camelCase alias or its own name, booleans in lax mode, no other
/// key, and every violation reported rather than the first.
fn rewind_params(
    params: &BTreeMap<String, Value>,
    with_options: bool,
) -> Result<RewindParams, ParamsRejection> {
    let mut fields = vec![("sessionId", "session_id"), ("entryId", "entry_id")];
    if with_options {
        fields.extend([("restoreFiles", "restore_files"), ("inplace", "inplace")]);
    }
    let mut issues = Vec::new();
    let field = |alias: &str, name: &str| params.get(alias).or_else(|| params.get(name));
    let mut text = |alias: &str, name: &str| match field(alias, name) {
        Some(Value::String(value)) => value.clone(),
        Some(_) => {
            issues.push(issue(alias, "Input should be a valid string"));
            String::new()
        }
        None => {
            issues.push(issue(alias, "Field required"));
            String::new()
        }
    };
    let session_id = text("sessionId", "session_id");
    let entry_id = text("entryId", "entry_id");
    let mut flag = |alias: &str, name: &str| match field(alias, name) {
        None => false,
        Some(value) => vibe_core::tools::python_bool(value).unwrap_or_else(|| {
            let message = if matches!(value, Value::String(_) | Value::Number(_)) {
                "Input should be a valid boolean, unable to interpret input"
            } else {
                "Input should be a valid boolean"
            };
            issues.push(issue(alias, message));
            false
        }),
    };
    let (restore_files, inplace) = if with_options {
        (
            flag("restoreFiles", "restore_files"),
            flag("inplace", "inplace"),
        )
    } else {
        (false, false)
    };
    for key in params.keys() {
        if !fields
            .iter()
            .any(|(alias, name)| key == alias || key == name)
        {
            issues.push(issue(key, "Extra inputs are not permitted"));
        }
    }
    if issues.is_empty() {
        Ok(RewindParams {
            session_id,
            entry_id,
            restore_files,
            inplace,
        })
    } else {
        Err(ParamsRejection::with_issues(issues))
    }
}

fn issue(field: &str, message: &str) -> InvalidParamsIssue {
    InvalidParamsIssue {
        path: vec![PathSegment::Field(field.to_owned())],
        message: message.to_owned(),
    }
}

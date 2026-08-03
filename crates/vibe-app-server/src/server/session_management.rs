use super::*;
use crate::release3::Release3Dispatch;
use crate::session_lifecycle::{DeleteSessionError, delete_session_transactionally};
use std::path::PathBuf;
use vibe_core::workspace::{RestoreTransaction, WorkspaceError};

pub(super) fn dispatch(connection: &mut ServerConnection, request: ServerRequest) -> DispatchBatch {
    let target_session_id = target_session_id(&request);
    if let Some(conflict) = mutation_conflict(connection, &request, target_session_id.as_deref()) {
        return conflict;
    }
    if request.method == "session/delete" {
        return delete_session(connection, request, target_session_id.as_deref());
    }

    let mut rewind =
        match RewindTransaction::prepare(connection, &request, target_session_id.as_deref()) {
            Ok(rewind) => rewind,
            Err(batch) => return batch,
        };
    let config_scope = request
        .method
        .starts_with("config/")
        .then(|| {
            target_session_id.as_deref().and_then(|session_id| {
                connection.server.lock_sessions().ok().and_then(|sessions| {
                    session_by_id_or_alias(&sessions, session_id).map(|session| {
                        (
                            PathBuf::from(&session.working_directory),
                            session.intent.trusted,
                        )
                    })
                })
            })
        })
        .flatten();
    let dispatched = if request.method == "session/rewind" {
        connection
            .server
            .release3
            .rewind_after_workspace_restore(&request.params)
    } else if let Some((working_directory, project_trusted)) = config_scope {
        connection.server.release3.dispatch_scoped(
            &request.method,
            &request.params,
            working_directory,
            project_trusted,
        )
    } else {
        connection
            .server
            .release3
            .dispatch(&request.method, &request.params)
    };

    match dispatched {
        Ok(mut dispatch) => {
            let result_session_id = dispatch
                .attachment
                .as_ref()
                .map(|attachment| attachment.id.clone());
            if let Some(attachment) = &dispatch.attachment {
                let attached = if connection.attached_sessions.contains(&attachment.id) {
                    connection
                        .server
                        .refresh_release3_runtime(attachment, rewind.review())
                } else {
                    connection
                        .server
                        .attach_release3_runtime(attachment, rewind.review())
                        .map(|()| {
                            connection.attached_sessions.insert(attachment.id.clone());
                        })
                };
                if let Err(error) = attached {
                    return rewind_failure(
                        connection,
                        request.id,
                        error.to_string(),
                        rewind,
                        result_session_id.as_deref(),
                    );
                }
            }
            if request.method == "session/rewind/read"
                && let Some(session_id) = target_session_id.as_deref()
                && let Err(error) = enrich_rewind_targets(connection, session_id, &mut dispatch)
            {
                return internal_error_batch(request.id, &error);
            }
            if request.method == "session/agent/update" {
                update_runtime_agent(connection, &request);
            }
            if let Some(paths) = rewind.commit_workspace() {
                dispatch
                    .result
                    .insert("restoredPaths".to_owned(), json!(paths));
                dispatch
                    .result
                    .insert("restoreErrors".to_owned(), json!([]));
            }
            success_batch(request.id, dispatch.result)
        }
        Err(error) => {
            if let Err(rollback) = rewind.rollback_workspace() {
                return internal_error_batch(
                    request.id,
                    &ServerError::Resource(format!(
                        "session rewind failed ({error}); workspace rollback failed ({rollback})"
                    )),
                );
            }
            release3_error_batch(request.id, error)
        }
    }
}

fn target_session_id(request: &ServerRequest) -> Option<String> {
    request
        .params
        .get("sessionId")
        .and_then(Value::as_str)
        .or_else(|| {
            request
                .params
                .get("sourceSessionId")
                .and_then(Value::as_str)
        })
        .map(ToOwned::to_owned)
}

fn mutation_conflict(
    connection: &ServerConnection,
    request: &ServerRequest,
    session_id: Option<&str>,
) -> Option<DispatchBatch> {
    if !matches!(
        request.method.as_str(),
        "session/agent/update" | "session/fork" | "session/history/clear" | "session/rewind"
    ) {
        return None;
    }
    let session_id = session_id?;
    let sessions = match connection.server.lock_sessions() {
        Ok(sessions) => sessions,
        Err(error) => return Some(internal_error_batch(request.id.clone(), &error)),
    };
    session_by_id_or_alias(&sessions, session_id)
        .is_some_and(|session| session.active_turn.is_some())
        .then(|| {
            error_batch(
                request.id.clone(),
                ProtocolErrorCode::Conflict,
                "Session agent and history can only change while the session is idle",
            )
        })
}

fn delete_session(
    connection: &mut ServerConnection,
    request: ServerRequest,
    session_id: Option<&str>,
) -> DispatchBatch {
    let Some(session_id) = session_id else {
        return error_batch(
            request.id,
            ProtocolErrorCode::InvalidParams,
            "session/delete requires sessionId",
        );
    };
    let attached = match connection.server.lock_sessions() {
        Ok(sessions) => session_by_id_or_alias(&sessions, session_id)
            .is_some_and(|session| session.attachments > 0),
        Err(error) => return internal_error_batch(request.id, &error),
    };
    if attached {
        return error_batch(
            request.id,
            ProtocolErrorCode::Conflict,
            "An attached session cannot be deleted",
        );
    }
    match delete_session_transactionally(&connection.server.release4, session_id, || {
        connection
            .server
            .release3
            .dispatch(&request.method, &request.params)
    }) {
        Ok(dispatch) => success_batch(request.id, dispatch.result),
        Err(DeleteSessionError::Prepare(error)) => release4_error_batch(request.id, error),
        Err(DeleteSessionError::Delete(error)) => release3_error_batch(request.id, error),
        Err(DeleteSessionError::Rollback { delete, rollback }) => internal_error_batch(
            request.id,
            &ServerError::Release4(format!(
                "session delete failed ({delete}); rollback failed ({rollback})"
            )),
        ),
    }
}

fn enrich_rewind_targets(
    connection: &ServerConnection,
    session_id: &str,
    dispatch: &mut Release3Dispatch,
) -> Result<(), ServerError> {
    let sessions = connection.server.lock_sessions()?;
    let review = {
        session_by_id_or_alias(&sessions, session_id).and_then(|session| session.review.clone())
    };
    drop(sessions);
    let Some(review) = review else {
        return Ok(());
    };
    let mut restore_supported = false;
    if let Some(messages) = dispatch
        .result
        .get_mut("messages")
        .and_then(Value::as_array_mut)
    {
        for message in messages {
            let Some(index) = message
                .get("messageIndex")
                .and_then(Value::as_u64)
                .and_then(|index| usize::try_from(index).ok())
            else {
                continue;
            };
            let paths = review
                .restorable_paths_at(index)
                .map_err(|error| ServerError::Resource(error.to_string()))?;
            let has_changes = !paths.is_empty();
            restore_supported |= has_changes;
            message["hasFileChanges"] = json!(has_changes);
            message["restorablePaths"] = json!(paths);
        }
    }
    dispatch
        .result
        .insert("restoreSupported".to_owned(), json!(restore_supported));
    Ok(())
}

fn update_runtime_agent(connection: &ServerConnection, request: &ServerRequest) {
    let (Some(session_id), Some(agent)) = (
        request.params.get("sessionId").and_then(Value::as_str),
        request.params.get("name").and_then(Value::as_str),
    ) else {
        return;
    };
    if let Ok(mut sessions) = connection.server.lock_sessions()
        && let Some(session) = session_mut_by_id_or_alias(&mut sessions, session_id)
    {
        session.intent.agent = Some(agent.to_owned());
        session.updated_at = now_millis();
    }
}

#[derive(Default)]
struct RewindTransaction {
    source: Option<HydratedSession>,
    rewound_review: Option<Arc<ReviewManager>>,
    workspace: Option<RestoreTransaction>,
}

impl RewindTransaction {
    fn prepare(
        connection: &ServerConnection,
        request: &ServerRequest,
        session_id: Option<&str>,
    ) -> Result<Self, DispatchBatch> {
        if request.method != "session/rewind" {
            return Ok(Self::default());
        }
        let Some(session_id) = session_id else {
            return Ok(Self::default());
        };
        let message_index = request
            .params
            .get("messageIndex")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok());
        let source = connection
            .server
            .release3
            .snapshot_session(session_id)
            .map_err(|error| release3_error_batch(request.id.clone(), error))?;
        let sessions = connection
            .server
            .lock_sessions()
            .map_err(|error| internal_error_batch(request.id.clone(), &error))?;
        let review = {
            session_by_id_or_alias(&sessions, session_id).and_then(|session| session.review.clone())
        };
        drop(sessions);
        let rewound_review = review
            .as_ref()
            .zip(message_index)
            .map(|(review, message_index)| review.fork_at(message_index))
            .transpose()
            .map_err(|error| {
                internal_error_batch(
                    request.id.clone(),
                    &ServerError::Resource(error.to_string()),
                )
            })?
            .map(Arc::new);
        let restore_requested = request
            .params
            .get("restoreFiles")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let workspace = if restore_requested {
            let Some(message_index) = message_index else {
                return Err(error_batch(
                    request.id.clone(),
                    ProtocolErrorCode::InvalidParams,
                    "file restoration requires messageIndex",
                ));
            };
            let Some(review) = review.as_ref() else {
                return Err(error_batch(
                    request.id.clone(),
                    ProtocolErrorCode::InvalidParams,
                    "this session has no restorable file checkpoint",
                ));
            };
            Some(
                review
                    .stage_restore_to_message(message_index)
                    .map_err(|error| {
                        error_batch(
                            request.id.clone(),
                            ProtocolErrorCode::InvalidParams,
                            &format!("Rewind failed: {error}"),
                        )
                    })?,
            )
        } else {
            None
        };
        Ok(Self {
            source: Some(source),
            rewound_review,
            workspace,
        })
    }

    fn review(&self) -> Option<Arc<ReviewManager>> {
        self.rewound_review.clone()
    }

    fn commit_workspace(&mut self) -> Option<Vec<String>> {
        self.workspace.take().map(RestoreTransaction::commit)
    }

    fn rollback_workspace(&mut self) -> Result<(), WorkspaceError> {
        self.workspace
            .take()
            .map(RestoreTransaction::rollback)
            .transpose()
            .map(drop)
    }

    fn rollback(
        mut self,
        connection: &ServerConnection,
        result_session_id: Option<&str>,
    ) -> Result<(), String> {
        let mut failures = Vec::new();
        if let (Some(source), Some(result_session_id)) = (self.source.take(), result_session_id)
            && let Err(error) = connection
                .server
                .release3
                .rollback_rewind(source, result_session_id)
        {
            failures.push(format!("session rollback failed ({error})"));
        }
        if let Err(error) = self.rollback_workspace() {
            failures.push(format!("workspace rollback failed ({error})"));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }
}

fn rewind_failure(
    connection: &ServerConnection,
    request_id: RequestId,
    cause: String,
    rewind: RewindTransaction,
    result_session_id: Option<&str>,
) -> DispatchBatch {
    match rewind.rollback(connection, result_session_id) {
        Ok(()) => internal_error_batch(request_id, &ServerError::Resource(cause)),
        Err(rollback) => internal_error_batch(
            request_id,
            &ServerError::Resource(format!("{cause}; {rollback}")),
        ),
    }
}

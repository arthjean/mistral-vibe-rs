use super::*;
use crate::session_lifecycle::{DeleteSessionError, delete_session_transactionally};
use crate::workspace::WorkspaceDispatch;
use std::path::PathBuf;

/// What the session layer does around a workspace method.
///
/// The dispatcher used to test the method name at six points spread over three
/// phases, and twice for the same method. Every rule is stated once here, so the
/// flow below reads one plan instead of re-deriving it as it goes.
#[derive(Clone, Copy)]
struct MethodPlan {
    dispatch: Dispatch,
    /// What the answer needs before it reaches the wire, in the order the
    /// reference applies it.
    after: &'static [After],
}

/// Which entry point of the workspace service answers the method.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dispatch {
    /// The service answers from the process configuration alone.
    Plain,
    /// The service answers against the attached session's directory and trust,
    /// so a project file the session can see participates in the layering.
    SessionScoped,
}

/// One step the server applies to a workspace answer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum After {
    /// The agent the runtime now runs under, written back to the saved session.
    PersistAgent,
    /// The checkpoint log of the session a clear continues under, which starts
    /// with no message for a turn to be numbered against.
    ClearCheckpointLog,
    /// The runtime and the stripped-image count a configuration answer carries.
    ConfigContext,
    /// The active agent an agent listing reports alongside the catalog.
    ActiveAgent,
}

/// The plan for `method`.
fn plan(method: &str) -> MethodPlan {
    match method {
        "session/agent/update" => MethodPlan {
            dispatch: Dispatch::Plain,
            after: &[After::PersistAgent],
        },
        "session/history/clear" => MethodPlan {
            dispatch: Dispatch::Plain,
            after: &[After::ClearCheckpointLog],
        },
        "agents/list" | "agents/install" => MethodPlan {
            dispatch: Dispatch::Plain,
            after: &[After::ActiveAgent],
        },
        "config/read" | "config/patch" | "config/reload" | "config/thinking/write" => MethodPlan {
            dispatch: Dispatch::SessionScoped,
            after: &[After::ConfigContext],
        },
        method if method.starts_with("config/") => MethodPlan {
            dispatch: Dispatch::SessionScoped,
            after: &[],
        },
        _ => MethodPlan {
            dispatch: Dispatch::Plain,
            after: &[],
        },
    }
}

pub(super) fn dispatch(connection: &mut ServerConnection, request: ServerRequest) -> DispatchBatch {
    let target_session_id = target_session_id(&request);
    if let Some(conflict) = mutation_conflict(connection, &request, target_session_id.as_deref()) {
        return conflict;
    }
    // Deleting is the one method that does not answer from a dispatch: it runs
    // a transaction across two stores and reports what the transaction did.
    if request.method == "session/delete" {
        return delete_session(connection, request, target_session_id.as_deref());
    }
    let plan = plan(&request.method);
    let dispatched = dispatch_workspace(connection, &request, plan, target_session_id.as_deref());
    let mut dispatch = match dispatched {
        Ok(dispatch) => dispatch,
        Err(error) => return workspace_error_batch(request.id, error),
    };
    let result_session_id = dispatch
        .attachment
        .as_ref()
        .map(|attachment| attachment.id.clone());
    let newly_attached = match attach_runtime(connection, &dispatch, None) {
        Ok(newly_attached) => newly_attached,
        Err(error) => return internal_error_batch(request.id, &error),
    };
    for step in plan.after {
        let session_id = match step {
            // A clear continues under a new identifier, whose log starts empty;
            // the session it replaced keeps the log its turns are numbered by.
            After::ClearCheckpointLog => result_session_id.as_deref(),
            _ => target_session_id.as_deref(),
        };
        let Some(session_id) = session_id else {
            continue;
        };
        if let Err(error) = apply_after(connection, *step, session_id, &request, &mut dispatch) {
            return internal_error_batch(request.id, &error);
        }
    }
    let mut batch = success_batch(request.id, dispatch.result);
    // The snapshot follows the answer rather than preceding it: the reference
    // flushes what attachment buffered once the response is on the wire, so the
    // client reads its state after the call it made.
    if let Some(session_id) = newly_attached {
        batch
            .outbound
            .extend(connection.attachment_frames(&session_id));
    }
    batch
}

/// Runs the method through the entry point its plan names.
fn dispatch_workspace(
    connection: &ServerConnection,
    request: &ServerRequest,
    plan: MethodPlan,
    target_session_id: Option<&str>,
) -> Result<WorkspaceDispatch, WorkspaceServiceError> {
    let workspace = &connection.server.workspace;
    match plan.dispatch {
        Dispatch::Plain => workspace.dispatch(&request.method, &request.params),
        Dispatch::SessionScoped => match session_scope(connection, target_session_id) {
            Some((working_directory, project_trusted)) => workspace.dispatch_scoped(
                &request.method,
                &request.params,
                working_directory,
                project_trusted,
            ),
            // A connection with no session attached has no project layer to
            // read, so the process configuration answers on its own.
            None => workspace.dispatch(&request.method, &request.params),
        },
    }
}

/// The directory and the trust a scoped dispatch layers over.
fn session_scope(
    connection: &ServerConnection,
    target_session_id: Option<&str>,
) -> Option<(PathBuf, bool)> {
    let sessions = connection.server.lock_sessions().ok()?;
    sessions.get(target_session_id?).map(|session| {
        (
            PathBuf::from(&session.working_directory),
            session.intent.trusted,
        )
    })
}

/// Binds the runtime a dispatch attached, reporting the identifier when the
/// connection had not seen it before.
fn attach_runtime(
    connection: &mut ServerConnection,
    dispatch: &WorkspaceDispatch,
    review: Option<Arc<ReviewManager>>,
) -> Result<Option<String>, ServerError> {
    let Some(attachment) = &dispatch.attachment else {
        return Ok(None);
    };
    if connection.attached_sessions.contains(&attachment.id) {
        connection
            .server
            .refresh_workspace_runtime(attachment, review)?;
        return Ok(None);
    }
    connection
        .server
        .attach_workspace_runtime(attachment, review)?;
    connection.attached_sessions.insert(attachment.id.clone());
    Ok(Some(attachment.id.clone()))
}

/// Applies one plan step to the answer.
fn apply_after(
    connection: &ServerConnection,
    step: After,
    session_id: &str,
    request: &ServerRequest,
    dispatch: &mut WorkspaceDispatch,
) -> Result<(), ServerError> {
    match step {
        After::PersistAgent => {
            update_runtime_agent(connection, request);
            Ok(())
        }
        // The session a clear continues under starts with an empty list, so
        // its log starts empty too.
        After::ClearCheckpointLog => reset_checkpoint_log(connection, session_id, 0),
        After::ConfigContext => {
            enrich_config_response(connection, session_id, &request.method, dispatch);
            Ok(())
        }
        After::ActiveAgent => {
            enrich_active_agent(connection, session_id, dispatch);
            Ok(())
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
        "session/agent/update" | "session/fork" | "session/history/clear"
    ) {
        return None;
    }
    let session_id = session_id?;
    let sessions = match connection.server.lock_sessions() {
        Ok(sessions) => sessions,
        Err(error) => return Some(internal_error_batch(request.id.clone(), &error)),
    };
    sessions
        .get(session_id)
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
        Ok(sessions) => sessions
            .get(session_id)
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
    match delete_session_transactionally(&connection.server.projects, session_id, || {
        connection
            .server
            .workspace
            .dispatch(&request.method, &request.params)
    }) {
        Ok(dispatch) => success_batch(request.id, dispatch.result),
        Err(DeleteSessionError::Prepare(error)) => projects_error_batch(request.id, error),
        Err(DeleteSessionError::Delete(error)) => workspace_error_batch(request.id, error),
        Err(DeleteSessionError::Rollback { delete, rollback }) => internal_error_batch(
            request.id,
            &ServerError::Projects(format!(
                "session delete failed ({delete}); rollback failed ({rollback})"
            )),
        ),
    }
}

/// Names the agent the addressed session runs, rather than the one a fresh
/// session would.
///
/// The catalog service knows what is installed; only the server knows which
/// profile this session is running, and `AgentsListResponse.active` is that one.
fn enrich_active_agent(
    connection: &ServerConnection,
    session_id: &str,
    dispatch: &mut WorkspaceDispatch,
) {
    let summary = connection.server.lock_sessions().ok().and_then(|sessions| {
        sessions
            .get(session_id)
            .and_then(|session| session.agent_summary.clone())
    });
    if let Some(summary) = summary {
        dispatch.result.insert("active".to_owned(), summary);
    }
}

/// Fills in the parts of a configuration answer only the server can compose.
///
/// The configuration service knows what it wrote; the runtime a client reads
/// the result from is assembled from three owners, and the images a model can
/// no longer read are counted against the session's own history. Both are added
/// here so the service stays session-agnostic.
fn enrich_config_response(
    connection: &ServerConnection,
    session_id: &str,
    method: &str,
    dispatch: &mut WorkspaceDispatch,
) {
    // A read reports the configuration as it stands; a write also reports the
    // runtime the write produced.
    let runtime = method != "config/read";
    if !runtime {
        for (field, value) in connection.server.harness_selection().config_read_fields() {
            dispatch.result.insert(field.to_owned(), value);
        }
    }
    if runtime && let Some(snapshot) = connection.server.runtime_snapshot(session_id) {
        dispatch.result.insert("runtime".to_owned(), snapshot);
    }
    dispatch.result.insert(
        "strippedHistoryImages".to_owned(),
        json!(connection.server.stripped_history_images(session_id)),
    );
}

/// Empties the session's checkpoint log because the message list it is
/// numbered against was replaced, reopening a turn at `message_count` when one
/// was running.
pub(super) fn reset_checkpoint_log(
    connection: &ServerConnection,
    session_id: &str,
    message_count: usize,
) -> Result<(), ServerError> {
    let review = {
        let sessions = connection.server.lock_sessions()?;
        sessions
            .get(session_id)
            .and_then(|session| session.review.clone())
    };
    let Some(review) = review else {
        return Ok(());
    };
    review
        .reset_messages(message_count)
        .map_err(|error| ServerError::Resource(error.to_string()))
}

fn update_runtime_agent(connection: &ServerConnection, request: &ServerRequest) {
    let (Some(session_id), Some(agent)) = (
        request.params.get("sessionId").and_then(Value::as_str),
        request.params.get("name").and_then(Value::as_str),
    ) else {
        return;
    };
    let summary = connection
        .server
        .workspace
        .agent_profile(agent)
        .ok()
        .as_ref()
        .map(crate::workspace::agent_summary);
    if let Ok(mut sessions) = connection.server.lock_sessions()
        && let Some(session) = sessions.get_mut(session_id)
    {
        session.intent.agent = Some(agent.to_owned());
        session.agent_summary = summary;
        session.updated_at = now_millis();
    }
}

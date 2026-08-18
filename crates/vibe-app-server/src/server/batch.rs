//! Construction of the outbound frames a dispatch produces.
//!
//! Every domain error is mapped to its protocol code exactly once, here.

use super::*;

pub(super) fn success_batch(id: RequestId, result: BTreeMap<String, Value>) -> DispatchBatch {
    DispatchBatch {
        outbound: vec![success_bytes(id, result)],
        deferred: Vec::new(),
        close_after_flush: false,
    }
}

pub(super) fn success_bytes(id: RequestId, result: BTreeMap<String, Value>) -> Vec<u8> {
    encode_frame(&Envelope::Success(SuccessResponse {
        jsonrpc: JsonRpcVersion::V2,
        id,
        result,
    }))
}

pub(super) fn error_batch(id: RequestId, code: ProtocolErrorCode, message: &str) -> DispatchBatch {
    ProtocolFault::new(code, message).into_batch(id)
}

/// Frames a refusal under a code that carries no structured detail.
fn plain_error_batch(id: RequestId, code: ProtocolErrorCode, message: &str) -> DispatchBatch {
    let frame = Envelope::Error(ErrorResponse {
        jsonrpc: JsonRpcVersion::V2,
        id,
        error: ProtocolError {
            code,
            message: message.to_owned(),
            data: Value::Null,
        },
    });
    DispatchBatch {
        outbound: vec![encode_frame(&frame)],
        deferred: Vec::new(),
        close_after_flush: false,
    }
}

/// Answers a resource dispatch and publishes what it signaled.
///
/// The reference emits `runtime/updated` after any response that moved runtime
/// state, so the notification is built here from the live snapshot rather than
/// by the backend that raised the signal: the backend knows something changed,
/// the server knows what the changed runtime looks like.
pub(super) fn resource_result_batch(
    id: RequestId,
    server: &AppServer,
    session_id: &str,
    method: &str,
    result: Result<ResourceDispatch, ResourceError>,
) -> DispatchBatch {
    match result {
        Ok(mut dispatch) => {
            // What the backend learned about the session's integrations is
            // recorded before anything is composed from it, so the runtime this
            // answer and the notification carry are the state after the call.
            if let Some(state) = dispatch.signals.integrations.take()
                && let Ok(mut resources) = server.resources.lock()
            {
                resources.record_integrations(session_id, state);
            }
            // Every mutation answer that declares a runtime carries the one the
            // mutation produced, composed here for the same reason the
            // notification is: the backend knows something moved, the server
            // knows what the runtime looks like afterward.
            if RUNTIME_ANSWERS.contains(&method)
                && let Some(runtime) = server.runtime_snapshot(session_id)
            {
                dispatch.result.insert("runtime".to_owned(), runtime);
            }
            let mut outbound = vec![success_bytes(id, dispatch.result)];
            outbound.extend(signal_frames(server, session_id, &dispatch.signals));
            DispatchBatch {
                outbound,
                deferred: Vec::new(),
                close_after_flush: false,
            }
        }
        Err(error) => resource_error_batch(id, error),
    }
}

/// The resource methods whose response declares a `RuntimeSnapshot`.
const RUNTIME_ANSWERS: &[&str] = &[
    "connectors/refresh",
    "mcp/add",
    "mcp/login",
    "mcp/logout",
    "mcp/refresh",
    "mcp/toggle",
];

/// The notifications a dispatch's signals publish, in the reference's order.
pub(super) fn signal_frames(
    server: &AppServer,
    session_id: &str,
    signals: &ResourceSignals,
) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    if signals.runtime_updated
        && let Some(runtime) = server.runtime_snapshot(session_id)
    {
        frames.push(encode_notification(
            "runtime/updated",
            result_map([("sessionId", json!(session_id)), ("runtime", runtime)]),
        ));
    }
    if let Some(auth) = &signals.auth_url {
        frames.push(encode_notification(
            "mcp/authUrl",
            result_map([("name", json!(auth.name)), ("url", json!(auth.url))]),
        ));
    }
    for warning in &signals.warnings {
        frames.push(encode_notification(
            "warning",
            result_map([(
                "warning",
                json!({"message": warning, "code": null, "details": null}),
            )]),
        ));
    }
    frames
}

/// A refusal already resolved to the code and the message it answers with.
///
/// Every domain a dispatcher calls into raises its own error type, and the
/// boundary answers all of them in the same three shapes. Converting once, here,
/// is what lets a dispatcher propagate with `?` rather than match at every call,
/// and what keeps the mapping of a domain variant to a protocol code stated
/// once rather than once per site that can raise it.
pub(crate) enum ProtocolFault {
    /// A parameter rejection, which is the only refusal carrying structured
    /// detail: a client reads `errorCount` and `issues` on all of them.
    InvalidParams(ParamsRejection),
    Other {
        code: ProtocolErrorCode,
        message: String,
    },
}

impl ProtocolFault {
    /// A refusal a dispatcher raised by hand, under the code it names.
    pub(crate) fn new(code: ProtocolErrorCode, message: impl Into<String>) -> Self {
        let message = message.into();
        // Every `invalid_params` carries structured detail, wherever the
        // rejection was raised: the dispatchers check most parameters by hand
        // rather than through a deserializer, and a client reads the same shape
        // from all of them.
        if code == ProtocolErrorCode::InvalidParams {
            return Self::InvalidParams(ParamsRejection::at_root(message));
        }
        Self::Other { code, message }
    }

    pub(crate) fn invalid_params(message: impl Into<String>) -> Self {
        Self::InvalidParams(ParamsRejection::at_root(message.into()))
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::Other {
            code: ProtocolErrorCode::InternalError,
            message: message.into(),
        }
    }

    pub(crate) fn into_batch(self, id: RequestId) -> DispatchBatch {
        match self {
            Self::InvalidParams(rejection) => invalid_params_batch(id, rejection),
            Self::Other { code, message } => plain_error_batch(id, code, &message),
        }
    }
}

impl From<ParamsRejection> for ProtocolFault {
    fn from(rejection: ParamsRejection) -> Self {
        Self::InvalidParams(rejection)
    }
}

impl From<ResourceError> for ProtocolFault {
    fn from(error: ResourceError) -> Self {
        match error {
            ResourceError::MethodNotFound(message) => {
                Self::new(ProtocolErrorCode::MethodNotFound, message)
            }
            ResourceError::InvalidParams(message) => Self::invalid_params(message),
            ResourceError::NotFound(message) => Self::new(ProtocolErrorCode::NotFound, message),
            ResourceError::Conflict(message) | ResourceError::Unavailable(message) => {
                Self::new(ProtocolErrorCode::Conflict, message)
            }
        }
    }
}

impl From<Release3Error> for ProtocolFault {
    fn from(error: Release3Error) -> Self {
        match error {
            Release3Error::MethodNotFound(message) => {
                Self::new(ProtocolErrorCode::MethodNotFound, message)
            }
            Release3Error::InvalidParams(message) => Self::invalid_params(message),
            Release3Error::NotFound(message) => Self::new(ProtocolErrorCode::NotFound, message),
            Release3Error::Config(message)
            | Release3Error::Storage(message)
            | Release3Error::Extension(message)
            | Release3Error::Prompt(message) => Self::new(ProtocolErrorCode::Conflict, message),
            Release3Error::StatePoisoned | Release3Error::Json(_) => {
                Self::internal(error.to_string())
            }
        }
    }
}

impl From<Release4Error> for ProtocolFault {
    fn from(error: Release4Error) -> Self {
        match error {
            Release4Error::MethodNotFound(message) => {
                Self::new(ProtocolErrorCode::MethodNotFound, message)
            }
            Release4Error::InvalidParams(message) => Self::invalid_params(message),
            Release4Error::NotFound(message) => Self::new(ProtocolErrorCode::NotFound, message),
            Release4Error::Conflict(message) => Self::new(ProtocolErrorCode::Conflict, message),
            Release4Error::Cloud(crate::release4::CloudError::Unauthorized(message)) => {
                Self::new(ProtocolErrorCode::Unauthorized, message)
            }
            Release4Error::Cloud(error) => {
                Self::new(ProtocolErrorCode::Conflict, error.to_string())
            }
            Release4Error::VibeCode(_)
            | Release4Error::Persistence(_)
            | Release4Error::PersistenceState(_)
            | Release4Error::ProjectLinkPersistence(_)
            | Release4Error::ProjectLinkPersistenceState(_)
            | Release4Error::BackgroundTask
            | Release4Error::StatePoisoned
            | Release4Error::Json(_) => Self::internal(error.to_string()),
        }
    }
}

/// Every server-side failure is internal by the time it reaches the wire: the
/// refusals a client can act on are raised by the domains above, which carry
/// their own codes.
impl From<ServerError> for ProtocolFault {
    fn from(error: ServerError) -> Self {
        Self::internal(error.to_string())
    }
}

/// The refusal a method answers a session it cannot resolve with.
///
/// The wording is the caller's: the boundary spells this two ways and a client
/// matches on the message it already receives.
pub(super) fn session_missing(message: &'static str) -> ProtocolFault {
    ProtocolFault::new(ProtocolErrorCode::NotFound, message)
}

/// Answers `outcome`, converting a refusal into the frame it publishes.
pub(super) fn answered(
    id: RequestId,
    outcome: Result<DispatchBatch, ProtocolFault>,
) -> DispatchBatch {
    outcome.unwrap_or_else(|fault| fault.into_batch(id))
}

pub(super) fn resource_error_batch(id: RequestId, error: ResourceError) -> DispatchBatch {
    ProtocolFault::from(error).into_batch(id)
}

pub(super) fn release3_error_batch(id: RequestId, error: Release3Error) -> DispatchBatch {
    ProtocolFault::from(error).into_batch(id)
}

pub(super) fn release4_error_batch(id: RequestId, error: Release4Error) -> DispatchBatch {
    ProtocolFault::from(error).into_batch(id)
}

pub(super) fn release4_dispatch_batch(id: RequestId, dispatch: Release4Dispatch) -> DispatchBatch {
    let mut outbound = vec![success_bytes(id, dispatch.result)];
    for notification in dispatch.notifications {
        outbound.push(encode_notification(
            &notification.method,
            notification.params,
        ));
    }
    DispatchBatch {
        outbound,
        deferred: Vec::new(),
        close_after_flush: false,
    }
}

pub(super) fn internal_error_batch(id: RequestId, error: &ServerError) -> DispatchBatch {
    ProtocolFault::internal(error.to_string()).into_batch(id)
}

pub(super) fn encode_notification(method: &str, params: BTreeMap<String, Value>) -> Vec<u8> {
    encode_frame(&Envelope::Notification(Notification {
        jsonrpc: JsonRpcVersion::V2,
        method: notification_method(method).to_owned(),
        params,
    }))
}

/// Answers a request whose parameters were rejected, naming the value that
/// caused it.
///
/// Every `invalid_params` answer comes through here, whichever dispatcher raised
/// it, so a client reads the same `errorCount` and `issues` on all of them. Only
/// a deserialization failure knows a path; a dispatcher's own check reports at
/// the parameter object. A rejection under any other code carries no detail at
/// all, and the reference leaves `data` off the wire rather than sending null.
pub(super) fn invalid_params_batch(id: RequestId, rejection: ParamsRejection) -> DispatchBatch {
    let detail = InvalidParamsData {
        error_count: rejection.issues.len(),
        issues: rejection.issues,
    };
    let frame = Envelope::Error(ErrorResponse {
        jsonrpc: JsonRpcVersion::V2,
        id,
        error: ProtocolError {
            code: ProtocolErrorCode::InvalidParams,
            message: rejection.message,
            data: serde_json::to_value(detail).unwrap_or(Value::Null),
        },
    });
    DispatchBatch {
        outbound: vec![encode_frame(&frame)],
        deferred: Vec::new(),
        close_after_flush: false,
    }
}

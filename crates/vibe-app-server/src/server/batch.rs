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
    // Every `invalid_params` carries structured detail, wherever the rejection
    // was raised: the dispatchers check most parameters by hand rather than
    // through a deserializer, and a client reads the same shape from all of
    // them.
    if code == ProtocolErrorCode::InvalidParams {
        return invalid_params_batch(id, ParamsRejection::at_root(message.to_owned()));
    }
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

pub(super) fn resource_result_batch(
    id: RequestId,
    result: Result<ResourceDispatch, ResourceError>,
) -> DispatchBatch {
    match result {
        Ok(dispatch) => {
            let mut outbound = vec![success_bytes(id, dispatch.result)];
            if let Some(notification) = dispatch.notification {
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
        Err(error) => resource_error_batch(id, error),
    }
}

pub(super) fn resource_error_batch(id: RequestId, error: ResourceError) -> DispatchBatch {
    match error {
        ResourceError::MethodNotFound(message) => {
            error_batch(id, ProtocolErrorCode::MethodNotFound, &message)
        }
        ResourceError::InvalidParams(message) => {
            error_batch(id, ProtocolErrorCode::InvalidParams, &message)
        }
        ResourceError::NotFound(message) => error_batch(id, ProtocolErrorCode::NotFound, &message),
        ResourceError::Conflict(message) | ResourceError::Unavailable(message) => {
            error_batch(id, ProtocolErrorCode::Conflict, &message)
        }
    }
}

pub(super) fn release3_error_batch(id: RequestId, error: Release3Error) -> DispatchBatch {
    match error {
        Release3Error::MethodNotFound(message) => {
            error_batch(id, ProtocolErrorCode::MethodNotFound, &message)
        }
        Release3Error::InvalidParams(message) => {
            error_batch(id, ProtocolErrorCode::InvalidParams, &message)
        }
        Release3Error::NotFound(message) => error_batch(id, ProtocolErrorCode::NotFound, &message),
        Release3Error::Config(message)
        | Release3Error::Storage(message)
        | Release3Error::Extension(message)
        | Release3Error::Prompt(message) => error_batch(id, ProtocolErrorCode::Conflict, &message),
        Release3Error::StatePoisoned | Release3Error::Json(_) => {
            error_batch(id, ProtocolErrorCode::InternalError, &error.to_string())
        }
    }
}

pub(super) fn release4_error_batch(id: RequestId, error: Release4Error) -> DispatchBatch {
    match error {
        Release4Error::MethodNotFound(message) => {
            error_batch(id, ProtocolErrorCode::MethodNotFound, &message)
        }
        Release4Error::InvalidParams(message) => {
            error_batch(id, ProtocolErrorCode::InvalidParams, &message)
        }
        Release4Error::NotFound(message) => error_batch(id, ProtocolErrorCode::NotFound, &message),
        Release4Error::Conflict(message) => error_batch(id, ProtocolErrorCode::Conflict, &message),
        Release4Error::Cloud(crate::release4::CloudError::Unauthorized(message)) => {
            error_batch(id, ProtocolErrorCode::Unauthorized, &message)
        }
        Release4Error::Cloud(error) => {
            error_batch(id, ProtocolErrorCode::Conflict, &error.to_string())
        }
        Release4Error::Persistence(_)
        | Release4Error::PersistenceState(_)
        | Release4Error::ProjectLinkPersistence(_)
        | Release4Error::ProjectLinkPersistenceState(_)
        | Release4Error::BackgroundTask
        | Release4Error::StatePoisoned
        | Release4Error::Json(_) => {
            error_batch(id, ProtocolErrorCode::InternalError, &error.to_string())
        }
    }
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
    error_batch(id, ProtocolErrorCode::InternalError, &error.to_string())
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

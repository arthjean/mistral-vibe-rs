//! Construction of the outbound frames a dispatch produces.
//!
//! Every domain error is mapped to its protocol code exactly once, here.

use super::*;

pub(super) fn success_batch(id: RequestId, result: BTreeMap<String, Value>) -> DispatchBatch {
    DispatchBatch {
        outbound: success_bytes(id, result).into_iter().collect(),
        deferred: Vec::new(),
        close_after_flush: false,
    }
}

pub(super) fn success_bytes(
    id: RequestId,
    result: BTreeMap<String, Value>,
) -> Result<Vec<u8>, vibe_protocol::ProtocolValidationError> {
    encode_frame(&Envelope::Success(SuccessResponse {
        jsonrpc: JsonRpcVersion::V2,
        id,
        result,
    }))
}

pub(super) fn error_batch(id: RequestId, code: ProtocolErrorCode, message: &str) -> DispatchBatch {
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
        outbound: encode_frame(&frame).into_iter().collect(),
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
            let mut outbound = success_bytes(id, dispatch.result)
                .into_iter()
                .collect::<Vec<_>>();
            if let Some(notification) = dispatch.notification
                && let Ok(bytes) = encode_notification(&notification.method, notification.params)
            {
                outbound.push(bytes);
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
    let mut outbound = success_bytes(id.clone(), dispatch.result)
        .into_iter()
        .collect::<Vec<_>>();
    for notification in dispatch.notifications {
        match encode_notification(&notification.method, notification.params) {
            Ok(bytes) => outbound.push(bytes),
            Err(error) => return internal_error_batch(id, &error),
        }
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

pub(super) fn encode_notification(
    method: &str,
    params: BTreeMap<String, Value>,
) -> Result<Vec<u8>, ServerError> {
    Ok(encode_frame(&Envelope::Notification(Notification {
        jsonrpc: JsonRpcVersion::V2,
        method: method.to_owned(),
        params,
    }))?)
}

//! The frames that cross the transport, and the two functions that turn bytes
//! into one and back.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::ProtocolError;

/// The only JSON-RPC version this protocol speaks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum JsonRpcVersion {
    /// Serializes as `"2.0"`.
    #[default]
    #[serde(rename = "2.0")]
    V2,
}

/// Correlates a request with its response.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric identifier.
    Integer(i64),
    /// Opaque string identifier.
    String(String),
}

/// A message that expects no response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    /// Always `2.0`.
    pub jsonrpc: JsonRpcVersion,
    /// Notification name.
    pub method: String,
    /// Payload. Required: the reference declares it without a default, so an
    /// absent `params` is refused rather than read as empty. Measured against
    /// `validate_json_rpc_envelope` in `vibe/app_server/protocol.py`.
    pub params: BTreeMap<String, Value>,
}

/// A message that expects a [`SuccessResponse`] or an [`ErrorResponse`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerRequest {
    /// Always `2.0`.
    pub jsonrpc: JsonRpcVersion,
    /// Identifier echoed back in the response.
    pub id: RequestId,
    /// One of [`SERVER_METHODS`](crate::SERVER_METHODS), or a lifecycle method.
    pub method: String,
    /// Payload. Required, for the reason [`Notification::params`] states.
    pub params: BTreeMap<String, Value>,
}

/// Successful answer to a [`ServerRequest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuccessResponse {
    /// Always `2.0`.
    pub jsonrpc: JsonRpcVersion,
    /// Identifier of the answered request.
    pub id: RequestId,
    /// Method-specific payload. Required: a response carries `result` or
    /// `error`, never neither.
    pub result: BTreeMap<String, Value>,
}

/// Failed answer to a [`ServerRequest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorResponse {
    /// Always `2.0`.
    pub jsonrpc: JsonRpcVersion,
    /// Identifier of the answered request.
    pub id: RequestId,
    /// Failure detail.
    pub error: ProtocolError,
}

/// Any frame that can cross the transport, in either direction.
///
/// Variants are discriminated by the fields each one denies, not by ordering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Envelope {
    /// See [`Notification`].
    Notification(Notification),
    /// See [`ServerRequest`].
    Request(ServerRequest),
    /// See [`SuccessResponse`].
    Success(SuccessResponse),
    /// See [`ErrorResponse`].
    Error(ErrorResponse),
}

/// Why an inbound frame could not be turned into an [`Envelope`].
#[derive(Debug, Error)]
pub enum ProtocolValidationError {
    /// The bytes are not a valid frame. Untagged decoding cannot report which
    /// variant was intended, so the message names the whole envelope.
    #[error("invalid JSON frame: {0}")]
    Json(#[from] serde_json::Error),
}

/// Decodes an inbound frame.
///
/// A frame that fails here carries no usable `id`, so the protocol has no way
/// to answer it: callers close the connection instead of replying.
pub fn decode_frame(bytes: &[u8]) -> Result<Envelope, ProtocolValidationError> {
    Ok(serde_json::from_slice(bytes)?)
}

/// Serializes an outbound frame.
///
/// Every field of [`Envelope`] is JSON-native (strings, `i64`,
/// [`serde_json::Value`], unit enums and maps keyed by `String`), so
/// serialization has no failure mode and callers get bytes, not a `Result`
/// they would have to discard.
#[must_use]
#[expect(
    clippy::expect_used,
    reason = "Envelope is JSON-native by construction; serialization cannot fail"
)]
pub fn encode_frame(frame: &Envelope) -> Vec<u8> {
    serde_json::to_vec(frame).expect("Envelope serialization is infallible")
}

#[cfg(test)]
mod envelope_tests;

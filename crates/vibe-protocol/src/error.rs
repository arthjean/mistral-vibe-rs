//! The failure payload an [`ErrorResponse`](crate::ErrorResponse) carries.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Closed set of failure codes carried by [`ProtocolError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorCode {
    /// The frame is well-formed JSON but violates the lifecycle contract.
    InvalidRequest,
    /// The method exists but its parameters do not deserialize.
    InvalidParams,
    /// A method was called before the handshake completed.
    NotInitialized,
    /// The addressed resource does not exist.
    NotFound,
    /// The request conflicts with the current server state.
    Conflict,
    /// The addressed turn is no longer the active one.
    StaleTurn,
    /// The active turn cannot accept steering input.
    NotSteerable,
    /// Compaction did not produce a usable session.
    CompactionFailed,
    /// Credentials are missing or rejected.
    Unauthorized,
    /// Credentials are valid but the operation is not permitted.
    Forbidden,
    /// The method is not part of [`SERVER_METHODS`](crate::SERVER_METHODS).
    MethodNotFound,
    /// The server failed for a reason the client cannot act on.
    InternalError,
}

/// Error payload of an [`ErrorResponse`](crate::ErrorResponse).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolError {
    /// Machine-readable failure class.
    pub code: ProtocolErrorCode,
    /// Human-readable explanation.
    pub message: String,
    /// Optional structured detail.
    ///
    /// The reference models this as a nullable value with no serialization
    /// filter, so a detail-free error puts `"data": null` on the wire rather
    /// than dropping the key. Measured against `ProtocolError.model_dump` in
    /// `vibe/app_server/protocol.py`.
    #[serde(default)]
    pub data: Value,
}

/// One step of an [`InvalidParamsIssue`] path: a field name or an array index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PathSegment {
    /// Object key.
    Field(String),
    /// Array index.
    Index(usize),
}

/// One reason a request was rejected, pointing at the value that caused it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvalidParamsIssue {
    /// Field names and array indices leading to the offending value, outermost
    /// first. Empty when the failure is about the parameter object itself.
    pub path: Vec<PathSegment>,
    /// What was wrong with the value at `path`.
    pub message: String,
}

/// Structured detail carried by an `invalid_params` [`ProtocolError`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InvalidParamsData {
    /// How many issues `issues` carries. The reference reports the validator's
    /// own count alongside the list, so the field is part of the contract
    /// rather than a cache of `issues.len()`.
    pub error_count: usize,
    /// Every issue found, in the order they were detected.
    pub issues: Vec<InvalidParamsIssue>,
}

#[cfg(test)]
mod error_tests;

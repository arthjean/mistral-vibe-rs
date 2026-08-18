//! Wire contract for the Mistral Vibe app-server.
//!
//! The crate owns three things and nothing else: the JSON-RPC envelopes that
//! cross the transport ([`envelope`]), the inventory of methods the server
//! answers ([`methods`]), and the handshake payloads exchanged during
//! `initialize` ([`handshake`]). [`error`] holds the failure payload the
//! envelopes carry.
//!
//! Envelopes are deliberately stricter than JSON-RPC 2.0: every struct denies
//! unknown fields, which is what lets [`Envelope`] discriminate its variants
//! while staying untagged. Relaxing `deny_unknown_fields` on any of them would
//! make the declaration order of [`Envelope`] load-bearing, so
//! `exactly_one_envelope_claims_each_valid_frame` counts the variants that
//! accept each shape rather than leaving the invariant to this paragraph.

#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod envelope;
pub mod error;
pub mod handshake;
pub mod methods;

pub use envelope::{
    Envelope, ErrorResponse, JsonRpcVersion, Notification, ProtocolValidationError, RequestId,
    ServerRequest, SuccessResponse, decode_frame, encode_frame,
};
pub use error::{
    InvalidParamsData, InvalidParamsIssue, PathSegment, ProtocolError, ProtocolErrorCode,
};
pub use handshake::{
    CallbackKind, ClientCapabilities, ClientEntrypoint, ClientInfo, ClientToolCapability,
    InitializeParams, InitializeResponse, ProtocolVersion, ServerCapabilities, ServerInfo,
    TerminalEmulator, TransportKind,
};
pub use methods::{
    LOCAL_EXTENSION_METHODS, SERVER_METHODS, is_dispatchable_method, is_server_method,
};

//! The six review methods, answered from the session's checkpoint engine.
//!
//! Mirrors `vibe/app_server/_review.py` behind the guards its caller,
//! `CoreRequestHandler` (`vibe/app_server/_handler.py`), puts in front of it,
//! in the reference's order:
//!
//! 1. With no session on the connection, every method is refused as a
//!    conflict before its parameters are read, which is what the reference's
//!    server answers before it has a root to route to.
//! 2. While the session is in a lifecycle transition (a compaction), every
//!    method is refused as a conflict, reads included.
//! 3. The parameters are validated ([`params`]), every violation reported.
//! 4. The session named has to be the connection's own current session, or
//!    the answer is `not_found`.
//! 5. A decision needs an idle session.
//!
//! What the engine refuses keeps the reference's split between the two codes:
//! a decision refused while a turn is open and a file that could not be written
//! back are `invalid_params`, and an unknown region or an unreadable file is an
//! `internal_error` (see `ReviewError::is_request_failure`).
//!
//! The projection is `vibe_core::checkpoints` and the disk access is
//! [`ReviewManager`]; what is here is the routing, the guards and the JSON
//! shape. The wire spelling comes from the serde attributes on the core types,
//! so a field renamed here would have to be renamed where the engine declares
//! it, which is where the census reads it.

mod params;
#[cfg(test)]
mod params_tests;

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::{ParamsRejection, ProtocolFault, ServerRequest};
use params::{ReviewCall, ReviewRequest};
use vibe_core::checkpoints::ReviewError;
use vibe_core::workspace::ReviewManager;
use vibe_protocol::ProtocolErrorCode;

/// Whether `method` is one this module answers.
pub(super) fn is_review_method(method: &str) -> bool {
    matches!(
        method,
        "review/approve"
            | "review/baseline"
            | "review/hunks"
            | "review/revert"
            | "review/state"
            | "review/turnDiff"
    )
}

/// The refusal the reference's server answers any session method with before
/// a session is started, resumed or continued on the connection.
pub(super) fn no_session() -> ProtocolFault {
    ProtocolFault::plain(
        ProtocolErrorCode::Conflict,
        "Start, resume, or continue a session before using this method",
    )
}

/// The refusal every method gets while `session_id` is compacting.
///
/// Reference `CoreRequestHandler.dispatch`, which refuses everything while a
/// lifecycle execution is active, and `_compact`, which reserves one named
/// `compact:<session>`.
pub(super) fn in_lifecycle(session_id: &str) -> ProtocolFault {
    ProtocolFault::plain(
        ProtocolErrorCode::Conflict,
        format!("Session lifecycle transition is active: compact:{session_id}"),
    )
}

/// Validates `request`'s parameters.
///
/// # Errors
///
/// Answers every violation as one `invalid_params`.
pub(super) fn validate(request: &ServerRequest) -> Result<ReviewRequest, ProtocolFault> {
    params::parse(&request.method, &request.params)
        .map_err(|issues| ProtocolFault::InvalidParams(ParamsRejection::with_issues(issues)))
}

/// The refusal a decision gets while `turn_id` runs.
///
/// Reference `SessionExecution.require_idle`, which names the execution by
/// its kind and identifier.
pub(super) fn busy(turn_id: &str) -> ProtocolFault {
    ProtocolFault::plain(
        ProtocolErrorCode::Conflict,
        format!("Session is busy running turn {turn_id}"),
    )
}

/// Answers one validated request against `review`, the session's engine.
///
/// A session with no engine answers every read as empty rather than failing: a
/// workspace that could not be opened has no changes to review, and a client
/// polling the panel should see an empty panel, not an error it cannot act on.
///
/// # Errors
///
/// Reports what the engine refused, under the code the reference answers it
/// with.
pub(super) fn answer(
    call: &ReviewCall,
    review: Option<&ReviewManager>,
) -> Result<BTreeMap<String, Value>, ProtocolFault> {
    let Some(review) = review else {
        return Ok(empty_answer(call));
    };
    match call {
        ReviewCall::State => {
            let state = review.review_state().map_err(refused)?;
            Ok(entries([
                ("files", encoded(&state.files)?),
                ("scopes", encoded(&state.scopes)?),
            ]))
        }
        ReviewCall::Baseline { path } => {
            let content = review.baseline_text(path).map_err(refused)?;
            Ok(entries([("content", Value::String(content))]))
        }
        ReviewCall::Hunks { path, owner } => {
            let hunks = review.file_hunks(path, *owner).map_err(refused)?;
            Ok(entries([("hunks", encoded(&hunks)?)]))
        }
        ReviewCall::TurnDiff { path, owner } => {
            let diff = review.scope_file_diff(path, *owner).map_err(refused)?;
            Ok(entries([
                ("status", encoded(&diff.status)?),
                ("baseline", Value::String(diff.baseline)),
                ("current", Value::String(diff.current)),
            ]))
        }
        ReviewCall::Approve(target) => {
            review.approve_review(target).map_err(refused)?;
            Ok(BTreeMap::new())
        }
        ReviewCall::Revert(target) => {
            review.revert_review(target).map_err(refused)?;
            Ok(BTreeMap::new())
        }
    }
}

fn empty_answer(call: &ReviewCall) -> BTreeMap<String, Value> {
    match call {
        ReviewCall::State => entries([
            ("files", Value::Array(Vec::new())),
            ("scopes", Value::Array(Vec::new())),
        ]),
        ReviewCall::Baseline { .. } => entries([("content", Value::String(String::new()))]),
        ReviewCall::Hunks { .. } => entries([("hunks", Value::Array(Vec::new()))]),
        ReviewCall::TurnDiff { .. } => entries([
            ("status", Value::String("modified".to_owned())),
            ("baseline", Value::String(String::new())),
            ("current", Value::String(String::new())),
        ]),
        ReviewCall::Approve(_) | ReviewCall::Revert(_) => BTreeMap::new(),
    }
}

/// What the engine refused, in the code the reference answers it with.
fn refused(error: ReviewError) -> ProtocolFault {
    let code = if error.is_request_failure() {
        ProtocolErrorCode::InvalidParams
    } else {
        ProtocolErrorCode::InternalError
    };
    ProtocolFault::plain(code, error.to_string())
}

fn encoded<T: Serialize>(value: &T) -> Result<Value, ProtocolFault> {
    serde_json::to_value(value).map_err(|error| {
        ProtocolFault::internal(format!("review answer is not encodable: {error}"))
    })
}

fn entries<const N: usize>(pairs: [(&str, Value); N]) -> BTreeMap<String, Value> {
    pairs
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

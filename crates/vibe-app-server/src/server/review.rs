//! The six review methods, answered from the session's checkpoint engine.
//!
//! These used to be answered by [`crate::resources::ResourceService`] from a map
//! whose only writer was a test, so `review/state` published an empty file list
//! in every production session and the other five answered `NotFound` for every
//! path. The engine that knows what each turn changed was already being driven
//! by the turn boundaries; nothing consulted it. This module is the join.
//!
//! It sits at the server rather than in the resource service for the same reason
//! `runtime/read` and `stats/read` do: every one of the six reads a session, and
//! the service holds none. What is left here is thin on purpose. The projection
//! is `vibe_core::checkpoints`, the disk access is [`ReviewManager`], and this is
//! parameter validation, the idle gate and the JSON shape.
//!
//! Mirrors `vibe/app_server/_review.py`. The wire spelling comes from the serde
//! attributes on the core types, so a field renamed here would have to be
//! renamed where the engine declares it, which is where the census reads it.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::params;
use crate::resources::ResourceError;
use vibe_core::checkpoints::{Owner, ReviewError, ReviewTarget};
use vibe_core::workspace::ReviewManager;

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

/// Answers one review method against `review`, the session's engine.
///
/// A session with no engine answers every read as empty rather than failing: a
/// workspace that could not be opened has no changes to review, and a client
/// polling the panel should see an empty panel, not an error it cannot act on.
///
/// # Errors
///
/// Refuses a mutation while a turn is running, rejects a malformed target or
/// owner, and reports what the engine refused.
pub(super) fn dispatch(
    method: &str,
    params: &BTreeMap<String, Value>,
    review: Option<&ReviewManager>,
    session_active: bool,
) -> Result<BTreeMap<String, Value>, ResourceError> {
    match method {
        "review/state" => {
            let Some(review) = review else {
                return Ok(entries([
                    ("files", Value::Array(Vec::new())),
                    ("scopes", Value::Array(Vec::new())),
                ]));
            };
            let state = review.review_state().map_err(refused)?;
            Ok(entries([
                ("files", encoded(&state.files)?),
                ("scopes", encoded(&state.scopes)?),
            ]))
        }
        "review/baseline" => {
            let path = required_string(params, "path")?;
            let content = match review {
                None => String::new(),
                Some(review) => review.baseline_text(path).map_err(refused)?,
            };
            Ok(entries([("content", Value::String(content))]))
        }
        "review/hunks" => {
            let path = required_string(params, "path")?;
            let owner = optional_owner(params)?;
            let hunks = match review {
                None => Vec::new(),
                Some(review) => review.file_hunks(path, owner).map_err(refused)?,
            };
            Ok(entries([("hunks", encoded(&hunks)?)]))
        }
        "review/turnDiff" => {
            let path = required_string(params, "path")?;
            let owner = required_owner(params)?;
            let Some(review) = review else {
                return Ok(entries([
                    ("status", Value::String("modified".to_owned())),
                    ("baseline", Value::String(String::new())),
                    ("current", Value::String(String::new())),
                ]));
            };
            let diff = review.scope_file_diff(path, owner).map_err(refused)?;
            Ok(entries([
                ("status", encoded(&diff.status)?),
                ("baseline", Value::String(diff.baseline)),
                ("current", Value::String(diff.current)),
            ]))
        }
        "review/approve" => mutate(
            params,
            review,
            session_active,
            ReviewManager::approve_review,
        ),
        "review/revert" => mutate(params, review, session_active, ReviewManager::revert_review),
        _ => Err(ResourceError::MethodNotFound(method.to_owned())),
    }
}

/// Applies one decision, behind the reference's idle requirement.
///
/// The gate is the session's, not the engine's: the engine refuses a decision
/// while its own turn is open, but a client asking during a turn deserves the
/// conflict the reference answers rather than the log's internal complaint.
fn mutate(
    params: &BTreeMap<String, Value>,
    review: Option<&ReviewManager>,
    session_active: bool,
    decide: fn(&ReviewManager, &ReviewTarget) -> Result<Vec<String>, ReviewError>,
) -> Result<BTreeMap<String, Value>, ResourceError> {
    if session_active {
        return Err(ResourceError::Conflict(
            "review mutations require an idle session".to_owned(),
        ));
    }
    let target = target_param(params)?;
    if let Some(review) = review {
        decide(review, &target).map_err(refused)?;
    }
    Ok(BTreeMap::new())
}

/// The target a mutation names.
fn target_param(params: &BTreeMap<String, Value>) -> Result<ReviewTarget, ResourceError> {
    let target = params
        .get("target")
        .ok_or_else(|| ResourceError::InvalidParams("target is required".to_owned()))?;
    serde_json::from_value(target.clone())
        .map_err(|error| ResourceError::InvalidParams(format!("target is not valid: {error}")))
}

/// The owner a method requires.
fn required_owner(params: &BTreeMap<String, Value>) -> Result<Owner, ResourceError> {
    let owner = params
        .get("owner")
        .ok_or_else(|| ResourceError::InvalidParams("owner is required".to_owned()))?;
    serde_json::from_value(owner.clone())
        .map_err(|error| ResourceError::InvalidParams(format!("owner is not valid: {error}")))
}

/// The owner a method takes optionally, where absent and null both mean the
/// whole-file diff.
fn optional_owner(params: &BTreeMap<String, Value>) -> Result<Option<Owner>, ResourceError> {
    match params.get("owner") {
        None | Some(Value::Null) => Ok(None),
        Some(_) => required_owner(params).map(Some),
    }
}

fn required_string<'a>(
    params: &'a BTreeMap<String, Value>,
    key: &str,
) -> Result<&'a str, ResourceError> {
    params::required_string(params, key)
        .map_err(|error| ResourceError::InvalidParams(error.message()))
}

/// What the engine refused, in the code the reference answers it with.
fn refused(error: ReviewError) -> ResourceError {
    ResourceError::InvalidParams(error.to_string())
}

fn encoded<T: Serialize>(value: &T) -> Result<Value, ResourceError> {
    serde_json::to_value(value).map_err(|error| {
        ResourceError::Unavailable(format!("review answer is not encodable: {error}"))
    })
}

fn entries<const N: usize>(pairs: [(&str, Value); N]) -> BTreeMap<String, Value> {
    pairs
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

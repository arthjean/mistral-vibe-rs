//! Request parameter validation, as the reference performs it.
//!
//! The reference validates every request's parameters with pydantic before it
//! does anything else (`validate_wire` in `vibe/app_server/_model.py`), and a
//! client reads the outcome: an `invalid_params` error whose `data` lists every
//! issue, in the order pydantic-core found them, each with the path to the
//! offending value. This module walks the same schemas pydantic-core walks,
//! reduced to their structure by `scripts/parity/app_server_params.py` into
//! `wire_validation/schema.json`, and reproduces both the issues and the value
//! a successful validation produces: the lax coercions of pydantic's Python
//! mode (`" 5 "` read as the integer 5, `"yes"` as `true`) are part of what a
//! request means, so a handler reads the validated value rather than the raw
//! one.
//!
//! The input keeps the key order the client sent: pydantic reports extra keys
//! in that order, and the envelope's parameter map sorts them.
//!
//! Messages are pydantic's own, which are the library's rather than the
//! reference's. The six validators the reference writes itself are named in
//! the schema and implemented here in [`custom`]; the sentences they raise are
//! this port's own, as `NOTICE` requires.

mod custom;
mod json;
mod schema;
mod walk;

use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde_json::Value;
use vibe_protocol::{InvalidParamsIssue, PathSegment};

pub(crate) use json::{Json, ordered_params};
use schema::SCHEMA;
use walk::Walk;

/// One issue, as pydantic reports it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Issue {
    pub(crate) path: Vec<PathSegment>,
    /// Pydantic's error type, such as `missing` or `string_type`.
    pub(crate) kind: &'static str,
    pub(crate) message: String,
}

impl Issue {
    pub(crate) fn wire(&self) -> InvalidParamsIssue {
        InvalidParamsIssue {
            path: self.path.clone(),
            message: self.message.clone(),
        }
    }
}

/// Validates `params` as the reference validates `method`'s parameters.
///
/// `Ok` carries the validated value, with every coercion applied and only the
/// fields the client sent, which is what pydantic dumps with `exclude_unset`.
/// A method the reference declares without a model accepts any object.
pub(crate) fn validate_method(method: &str, params: &Json) -> Result<Value, Vec<Issue>> {
    match SCHEMA.methods.get(method) {
        Some(Some(model)) => validate_model_named(model, params),
        _ => Ok(params.clone().into_value()),
    }
}

/// Validates the parameters of a request no routing has validated, as the
/// reference validates `method`'s, and reads the validated value as `T`.
///
/// An in-process request reaches its handler without the stdio routing that
/// validates first, so the handler validates here, and a request the routing
/// passed passes again. The envelope sorted the keys, so extra keys are
/// reported in that order. `T` reads what the schema already checked, and
/// only a `T` that disagrees with the schema refuses it, at the root.
pub(crate) fn validate_params<T: DeserializeOwned>(
    method: &str,
    params: &BTreeMap<String, Value>,
) -> Result<T, Vec<InvalidParamsIssue>> {
    let input = Json::Object(
        params
            .iter()
            .map(|(key, value)| (key.clone(), Json::from_value(value)))
            .collect(),
    );
    let validated = validate_method(method, &input)
        .map_err(|issues| issues.iter().map(Issue::wire).collect::<Vec<_>>())?;
    serde_json::from_value(validated).map_err(|error| {
        vec![InvalidParamsIssue {
            path: Vec::new(),
            message: format!("Invalid request parameters: {error}"),
        }]
    })
}

/// Validates `params` as the reference validates a lifecycle frame's
/// parameters, such as `initialize`.
pub(crate) fn validate_lifecycle(frame: &str, params: &Json) -> Result<Value, Vec<Issue>> {
    match SCHEMA.lifecycle.get(frame) {
        Some(model) => validate_model_named(model, params),
        None => Ok(params.clone().into_value()),
    }
}

/// Validates `value` as the handler of `method` validates what it reads past
/// the parameters: as `field` of the model the schema names for it. Issues
/// are located from `value` itself.
pub(crate) fn validate_nested_field(
    method: &str,
    field: &str,
    value: &Value,
) -> Result<Value, Vec<Issue>> {
    let Some(schema) = SCHEMA
        .nested
        .get(method)
        .and_then(|model| SCHEMA.defs.get(model))
        .and_then(|model| {
            model
                .fields
                .iter()
                .find(|candidate| candidate.alias == field)
        })
        .map(|field| &field.schema)
    else {
        return Ok(value.clone());
    };
    let mut walk = Walk::default();
    let validated = walk.node(schema, &Json::from_value(value), &mut Vec::new());
    if walk.issues.is_empty() {
        Ok(validated.unwrap_or(Value::Null))
    } else {
        Err(walk.issues)
    }
}

/// Validates `input` against the named reference model.
pub(crate) fn validate_model_named(model: &str, input: &Json) -> Result<Value, Vec<Issue>> {
    let mut walk = Walk::default();
    let value = walk.model(model, input, &mut Vec::new(), false);
    if walk.issues.is_empty() {
        Ok(value.unwrap_or(Value::Null))
    } else {
        Err(walk.issues)
    }
}

#[cfg(test)]
mod in_process_tests;
#[cfg(test)]
mod wire_validation_tests;

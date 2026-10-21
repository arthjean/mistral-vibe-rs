//! Python's `==` over the JSON values a rollout carries.
//!
//! The variants are [`serde_json::Value`]s, whose objects keep the key order
//! the eval response carried because the workspace builds `serde_json` with
//! `preserve_order`; that order is load-bearing, since the reference resolves
//! an object- or array-valued variant by handing it to `json.dumps`
//! ([`crate::pycompat::dumps`]). What `serde_json` cannot answer is whether two
//! values are equal the way Python compares the objects `json.loads` decodes
//! them to, which is how the configuration variants drop a value equal to its
//! default.
//!
//! Reference: `vibe/core/experiments/manager.py` at the pinned commit.

use serde_json::Value;

/// Whether two values are equal as Python's `==` compares the objects
/// `json.loads` decodes them to.
///
/// Python's booleans are integers, so `False == 0` and `True == 1.0`; an
/// integer and a float compare by value; a dict compares regardless of key
/// order. Every other pair compares by kind and content. The configuration
/// variants drop a value equal to the declared default this way, so a rollout
/// forcing `0` onto a flag defaulting to `false` reaches no layer, as upstream.
pub(crate) fn python_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| python_eq(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, value)| {
                    right
                        .get(key)
                        .is_some_and(|candidate| python_eq(value, candidate))
                })
        }
        (left, right) => match (python_number(left), python_number(right)) {
            (Some(left), Some(right)) => left == right,
            _ => false,
        },
    }
}

/// A boolean or a number as the value Python compares it by.
fn python_number(value: &Value) -> Option<PythonNumber> {
    match value {
        Value::Bool(flag) => Some(PythonNumber::Integer(i128::from(*flag))),
        Value::Number(number) => number
            .as_i64()
            .map(i128::from)
            .or_else(|| number.as_u64().map(i128::from))
            .map(PythonNumber::Integer)
            .or_else(|| number.as_f64().map(PythonNumber::Float)),
        _ => None,
    }
}

/// A number as Python compares it: an integer exactly, a float by value.
#[derive(Clone, Copy)]
enum PythonNumber {
    Integer(i128),
    Float(f64),
}

impl PartialEq for PythonNumber {
    #[allow(clippy::cast_precision_loss, clippy::float_cmp)]
    fn eq(&self, other: &Self) -> bool {
        match (*self, *other) {
            (Self::Integer(left), Self::Integer(right)) => left == right,
            (Self::Float(left), Self::Float(right)) => left == right,
            (Self::Integer(integer), Self::Float(float))
            | (Self::Float(float), Self::Integer(integer)) => {
                float.fract() == 0.0 && integer as f64 == float
            }
        }
    }
}

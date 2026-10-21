//! How pydantic's Python mode reads a JSON scalar as an `int`, a `float` or a
//! `bool`: leniently unless the schema is strict, so `" 5 "` is the integer 5
//! and `"yes"` is `true`.

use vibe_core::pycompat::pydantic;

use super::Json;

/// Why pydantic refuses a scalar: its error type and its message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub(crate) kind: &'static str,
    pub(crate) message: &'static str,
}

const INT_TYPE: Refusal = Refusal {
    kind: "int_type",
    message: "Input should be a valid integer",
};

const FLOAT_TYPE: Refusal = Refusal {
    kind: "float_type",
    message: "Input should be a valid number",
};

const BOOL_TYPE: Refusal = Refusal {
    kind: "bool_type",
    message: "Input should be a valid boolean",
};

const BOOL_PARSING: Refusal = Refusal {
    kind: "bool_parsing",
    message: "Input should be a valid boolean, unable to interpret input",
};

/// `input` read as an `int`: lax mode also takes a boolean, a float with no
/// fractional part and a string of digits. Pydantic's integers are
/// unbounded; this one saturates at the `i128` range.
pub(crate) fn integer(input: &Json, strict: bool) -> Result<i128, Refusal> {
    match input {
        Json::Number(number) => {
            if let Some(value) = number.as_i64() {
                return Ok(i128::from(value));
            }
            if let Some(value) = number.as_u64() {
                return Ok(i128::from(value));
            }
            if strict {
                return Err(INT_TYPE);
            }
            integral(number.as_f64().unwrap_or(f64::NAN))
        }
        Json::Bool(value) if !strict => Ok(i128::from(*value)),
        Json::String(text) if !strict => pydantic::lax_int(text).ok_or(Refusal {
            kind: "int_parsing",
            message: "Input should be a valid integer, unable to parse string as an integer",
        }),
        _ => Err(INT_TYPE),
    }
}

/// A float read as an `int`, which it is only when finite and integral.
fn integral(value: f64) -> Result<i128, Refusal> {
    if !value.is_finite() {
        return Err(Refusal {
            kind: "finite_number",
            message: "Input should be a finite number",
        });
    }
    if value.fract() != 0.0 {
        return Err(Refusal {
            kind: "int_from_float",
            message: "Input should be a valid integer, got a number with a fractional part",
        });
    }
    // An integer literal past the 64-bit range reaches this side of the
    // envelope as a float, which is the only way to tell it holds no fraction.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the value is finite and integral; past the i128 range it saturates"
    )]
    Ok(value as i128)
}

/// `input` read as a `float`: lax mode also takes a boolean and a numeric
/// string. Whether an infinity or NaN is refused is the model's to say.
pub(crate) fn float(input: &Json, strict: bool) -> Result<f64, Refusal> {
    match input {
        Json::Number(number) => number.as_f64().ok_or(FLOAT_TYPE),
        Json::Bool(value) if !strict => Ok(if *value { 1.0 } else { 0.0 }),
        Json::String(text) if !strict => pydantic::lax_float(text).ok_or(Refusal {
            kind: "float_parsing",
            message: "Input should be a valid number, unable to parse string as a number",
        }),
        _ => Err(FLOAT_TYPE),
    }
}

/// `input` read as a `bool`: lax mode also takes 0 and 1, as integers or
/// floats, and the words [`pydantic::lax_bool`] knows.
pub(crate) fn boolean(input: &Json, strict: bool) -> Result<bool, Refusal> {
    match input {
        Json::Bool(value) => Ok(*value),
        // An integer other than 0 or 1 cannot be read as a boolean; a float
        // other than those two is not a boolean at all.
        Json::Number(number) if !strict => match (number.as_i64(), number.as_f64()) {
            (Some(0), _) | (None, Some(0.0)) => Ok(false),
            (Some(1), _) | (None, Some(1.0)) => Ok(true),
            (Some(_), _) => Err(BOOL_PARSING),
            _ => Err(BOOL_TYPE),
        },
        Json::String(text) if !strict => pydantic::lax_bool(text).ok_or(BOOL_PARSING),
        _ => Err(BOOL_TYPE),
    }
}

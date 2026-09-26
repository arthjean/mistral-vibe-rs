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

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::LazyLock;

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Number, Value};
use vibe_protocol::{InvalidParamsIssue, PathSegment};

/// The reduced pydantic-core schemas, compiled in.
const SCHEMA_SOURCE: &str = include_str!("wire_validation/schema.json");

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    #[expect(
        clippy::expect_used,
        reason = "the schema is compiled in and `the_compiled_schema_parses` pins that it parses"
    )]
    serde_json::from_str(SCHEMA_SOURCE).expect("the compiled-in wire schema parses")
});

/// A JSON value that keeps object keys in the order they were written.
///
/// A duplicated key keeps its first position and its last value, which is what
/// Python's `json.loads` produces.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Reads `value` without an order to keep, as a map the envelope already
    /// sorted.
    pub(crate) fn from_value(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(value) => Self::Bool(*value),
            Value::Number(value) => Self::Number(value.clone()),
            Value::String(value) => Self::String(value.clone()),
            Value::Array(items) => Self::Array(items.iter().map(Self::from_value).collect()),
            Value::Object(entries) => Self::Object(
                entries
                    .iter()
                    .map(|(key, value)| (key.clone(), Self::from_value(value)))
                    .collect(),
            ),
        }
    }

    pub(crate) fn into_value(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(value),
            Self::Number(value) => Value::Number(value),
            Self::String(value) => Value::String(value),
            Self::Array(items) => Value::Array(items.into_iter().map(Self::into_value).collect()),
            Self::Object(entries) => Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, value.into_value()))
                    .collect(),
            ),
        }
    }

    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Self::Object(entries) => entries
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Json, E> {
        Ok(Json::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Json, E> {
        Ok(Json::Number(value.into()))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Json, E> {
        Ok(Json::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Json, E> {
        Ok(Number::from_f64(value).map_or(Json::Null, Json::Number))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Json, E> {
        Ok(Json::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Json, E> {
        Ok(Json::String(value))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Json, D::Error> {
        Json::deserialize(deserializer)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Json, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = sequence.next_element()? {
            items.push(item);
        }
        Ok(Json::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let mut entries: Vec<(String, Json)> = Vec::new();
        while let Some((key, value)) = map.next_entry::<String, Json>()? {
            match entries.iter_mut().find(|(name, _)| *name == key) {
                Some(entry) => entry.1 = value,
                None => entries.push((key, value)),
            }
        }
        Ok(Json::Object(entries))
    }
}

/// The parameters a request frame carries, with their key order kept.
///
/// `None` when the frame does not parse or carries no `params` object; the
/// envelope has already refused such a frame by the time this is asked.
pub(crate) fn ordered_params(frame: &[u8]) -> Option<Json> {
    #[derive(Deserialize)]
    struct Frame {
        params: Option<Json>,
    }
    serde_json::from_slice::<Frame>(frame)
        .ok()
        .and_then(|frame| frame.params)
}

// --------------------------------------------------------------------------
// The schema
// --------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Schema {
    methods: BTreeMap<String, Option<String>>,
    lifecycle: BTreeMap<String, String>,
    /// The model a method's handler validates past its parameters.
    nested: BTreeMap<String, String>,
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the schema tests"))]
    functions: Vec<String>,
    defs: BTreeMap<String, Model>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Model {
    name: String,
    fields: Vec<Field>,
    extra: Extra,
    #[serde(default = "allow")]
    allow_inf_nan: bool,
    #[serde(default)]
    validators: Vec<Validator>,
}

fn allow() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct Field {
    alias: String,
    schema: Node,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Extra {
    Forbid,
    Ignore,
    Allow,
}

#[derive(Debug, Deserialize)]
struct Validator {
    function: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum Node {
    Ref {
        name: String,
    },
    Default {
        schema: Box<Node>,
    },
    Nullable {
        schema: Box<Node>,
    },
    FunctionAfter {
        function: String,
        schema: Box<Node>,
    },
    List {
        items: Box<Node>,
        min_length: Option<usize>,
        max_length: Option<usize>,
    },
    Dict {
        keys: Box<Node>,
        values: Box<Node>,
    },
    TaggedUnion {
        discriminator: String,
        choices: Vec<(String, Node)>,
    },
    Literal {
        expected: Vec<Value>,
    },
    Enum {
        values: Vec<Value>,
    },
    Str {
        #[serde(default)]
        strict: bool,
        min_length: Option<usize>,
        max_length: Option<usize>,
    },
    Int {
        #[serde(default)]
        strict: bool,
        ge: Option<f64>,
        gt: Option<f64>,
        le: Option<f64>,
        lt: Option<f64>,
    },
    Float {
        #[serde(default)]
        strict: bool,
        ge: Option<f64>,
        gt: Option<f64>,
        le: Option<f64>,
        lt: Option<f64>,
    },
    Bool {
        #[serde(default)]
        strict: bool,
    },
    None,
    Any,
    JsonValue,
}

// --------------------------------------------------------------------------
// Validation
// --------------------------------------------------------------------------

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

#[derive(Default)]
struct Walk {
    issues: Vec<Issue>,
    /// Whether the model being validated refuses infinities and NaN.
    finite: Vec<bool>,
}

impl Walk {
    fn fail(&mut self, path: &[PathSegment], kind: &'static str, message: impl Into<String>) {
        self.issues.push(Issue {
            path: path.to_vec(),
            kind,
            message: message.into(),
        });
    }

    fn model(
        &mut self,
        name: &str,
        input: &Json,
        path: &mut Vec<PathSegment>,
        from_attributes: bool,
    ) -> Option<Value> {
        let Some(model) = SCHEMA.defs.get(name) else {
            self.fail(path, "model_type", format!("Unknown model {name}"));
            return None;
        };
        let Json::Object(entries) = input else {
            if from_attributes {
                self.fail(
                    path,
                    "model_attributes_type",
                    "Input should be a valid dictionary or object to extract fields from",
                );
            } else {
                self.fail(
                    path,
                    "model_type",
                    format!(
                        "Input should be a valid dictionary or instance of {}",
                        model.name
                    ),
                );
            }
            return None;
        };
        let before = self.issues.len();
        self.finite.push(!model.allow_inf_nan);
        let mut output = serde_json::Map::new();
        for field in &model.fields {
            path.push(PathSegment::Field(field.alias.clone()));
            match input.get(&field.alias) {
                Some(value) => {
                    if let Some(validated) = self.node(&field.schema, value, path) {
                        output.insert(field.alias.clone(), validated);
                    }
                }
                None if matches!(field.schema, Node::Default { .. }) => {}
                None => self.fail(path, "missing", "Field required"),
            }
            path.pop();
        }
        for (key, value) in entries {
            if model.fields.iter().any(|field| field.alias == *key) {
                continue;
            }
            match model.extra {
                Extra::Forbid => {
                    path.push(PathSegment::Field(key.clone()));
                    self.fail(path, "extra_forbidden", "Extra inputs are not permitted");
                    path.pop();
                }
                Extra::Ignore => {}
                Extra::Allow => {
                    output.insert(key.clone(), value.clone().into_value());
                }
            }
        }
        self.finite.pop();
        if self.issues.len() > before {
            return None;
        }
        let mut value = Value::Object(output);
        for validator in &model.validators {
            match custom::run(&validator.function, value) {
                Ok(checked) => value = checked,
                Err(message) => {
                    self.fail(path, "value_error", format!("Value error, {message}"));
                    return None;
                }
            }
        }
        Some(value)
    }

    fn node(&mut self, node: &Node, input: &Json, path: &mut Vec<PathSegment>) -> Option<Value> {
        match node {
            Node::Ref { name } => self.model(name, input, path, false),
            Node::Default { schema } => self.node(schema, input, path),
            Node::Nullable { schema } => match input {
                Json::Null => Some(Value::Null),
                _ => self.node(schema, input, path),
            },
            Node::FunctionAfter { function, schema } => {
                let value = self.node(schema, input, path)?;
                match custom::run(function, value) {
                    Ok(value) => Some(value),
                    Err(message) => {
                        self.fail(path, "value_error", format!("Value error, {message}"));
                        None
                    }
                }
            }
            Node::List {
                items,
                min_length,
                max_length,
            } => self.list(items, *min_length, *max_length, input, path),
            Node::Dict { keys, values } => self.dict(keys, values, input, path),
            Node::TaggedUnion {
                discriminator,
                choices,
            } => self.tagged(discriminator, choices, input, path),
            Node::Literal { expected } => {
                let value = input.clone().into_value();
                if expected
                    .iter()
                    .any(|candidate| python_equal(candidate, &value))
                {
                    Some(value)
                } else {
                    self.fail(
                        path,
                        "literal_error",
                        format!("Input should be {}", expected_list(expected)),
                    );
                    None
                }
            }
            Node::Enum { values } => {
                let value = input.clone().into_value();
                if values.iter().any(|candidate| candidate == &value) {
                    Some(value)
                } else {
                    self.fail(
                        path,
                        "enum",
                        format!("Input should be {}", expected_list(values)),
                    );
                    None
                }
            }
            Node::Str {
                strict,
                min_length,
                max_length,
            } => self.string(*strict, *min_length, *max_length, input, path),
            Node::Int {
                strict,
                ge,
                gt,
                le,
                lt,
            } => {
                let value = self.integer(*strict, input, path)?;
                // Bounds compare the integer exactly while it fits a float's
                // mantissa, which every bound the reference declares does.
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "bounds are small integers the reference declares"
                )]
                let reading = value as f64;
                self.bounds(reading, *ge, *gt, *le, *lt, path)
                    .then(|| Value::Number(value.into()))
            }
            Node::Float {
                strict,
                ge,
                gt,
                le,
                lt,
            } => {
                let value = self.float(*strict, input, path)?;
                if !self.bounds(value, *ge, *gt, *le, *lt, path) {
                    return None;
                }
                Some(Number::from_f64(value).map_or(Value::Null, Value::Number))
            }
            Node::Bool { strict } => self.boolean(*strict, input, path).map(Value::Bool),
            Node::None => match input {
                Json::Null => Some(Value::Null),
                _ => {
                    self.fail(path, "none_required", "Input should be None");
                    None
                }
            },
            Node::Any | Node::JsonValue => Some(input.clone().into_value()),
        }
    }

    fn list(
        &mut self,
        items: &Node,
        min_length: Option<usize>,
        max_length: Option<usize>,
        input: &Json,
        path: &mut Vec<PathSegment>,
    ) -> Option<Value> {
        let Json::Array(entries) = input else {
            self.fail(path, "list_type", "Input should be a valid list");
            return None;
        };
        let before = self.issues.len();
        let mut output = Vec::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            path.push(PathSegment::Index(index));
            if let Some(value) = self.node(items, entry, path) {
                output.push(value);
            }
            path.pop();
        }
        if self.issues.len() > before {
            return None;
        }
        if let Some(minimum) = min_length
            && output.len() < minimum
        {
            self.fail(
                path,
                "too_short",
                format!(
                    "List should have at least {minimum} item{} after validation, not {}",
                    if minimum == 1 { "" } else { "s" },
                    output.len()
                ),
            );
            return None;
        }
        if let Some(maximum) = max_length
            && output.len() > maximum
        {
            self.fail(
                path,
                "too_long",
                format!(
                    "List should have at most {maximum} item{} after validation, not {}",
                    if maximum == 1 { "" } else { "s" },
                    output.len()
                ),
            );
            return None;
        }
        Some(Value::Array(output))
    }

    fn dict(
        &mut self,
        keys: &Node,
        values: &Node,
        input: &Json,
        path: &mut Vec<PathSegment>,
    ) -> Option<Value> {
        let Json::Object(entries) = input else {
            self.fail(path, "dict_type", "Input should be a valid dictionary");
            return None;
        };
        let before = self.issues.len();
        let mut output = serde_json::Map::new();
        for (key, value) in entries {
            path.push(PathSegment::Field(key.clone()));
            path.push(PathSegment::Field("[key]".to_owned()));
            let validated_key = self.node(keys, &Json::String(key.clone()), path);
            path.pop();
            let validated_value = self.node(values, value, path);
            path.pop();
            if let (Some(Value::String(key)), Some(value)) = (validated_key, validated_value) {
                output.insert(key, value);
            }
        }
        (self.issues.len() == before).then_some(Value::Object(output))
    }

    fn tagged(
        &mut self,
        discriminator: &str,
        choices: &[(String, Node)],
        input: &Json,
        path: &mut Vec<PathSegment>,
    ) -> Option<Value> {
        if !matches!(input, Json::Object(_)) {
            self.fail(
                path,
                "model_attributes_type",
                "Input should be a valid dictionary or object to extract fields from",
            );
            return None;
        }
        let Some(tag) = input.get(discriminator) else {
            self.fail(
                path,
                "union_tag_not_found",
                format!("Unable to extract tag using discriminator '{discriminator}'"),
            );
            return None;
        };
        let chosen = match tag {
            Json::String(tag) => choices
                .iter()
                .find(|(name, _)| name == tag)
                .map(|(name, node)| (name.clone(), node)),
            _ => None,
        };
        let Some((name, node)) = chosen else {
            let expected = choices
                .iter()
                .map(|(name, _)| format!("'{name}'"))
                .collect::<Vec<_>>()
                .join(", ");
            self.fail(
                path,
                "union_tag_invalid",
                format!(
                    "Input tag '{}' found using '{discriminator}' does not match any of the \
                     expected tags: {expected}",
                    python_repr_bare(tag)
                ),
            );
            return None;
        };
        path.push(PathSegment::Field(name));
        let value = match node {
            Node::Ref { name } => self.model(name, input, path, true),
            other => self.node(other, input, path),
        };
        path.pop();
        value
    }

    fn string(
        &mut self,
        strict: bool,
        min_length: Option<usize>,
        max_length: Option<usize>,
        input: &Json,
        path: &mut [PathSegment],
    ) -> Option<Value> {
        let _ = strict;
        let Json::String(text) = input else {
            self.fail(path, "string_type", "Input should be a valid string");
            return None;
        };
        let length = text.chars().count();
        if let Some(minimum) = min_length
            && length < minimum
        {
            self.fail(
                path,
                "string_too_short",
                format!(
                    "String should have at least {minimum} character{}",
                    if minimum == 1 { "" } else { "s" }
                ),
            );
            return None;
        }
        if let Some(maximum) = max_length
            && length > maximum
        {
            self.fail(
                path,
                "string_too_long",
                format!(
                    "String should have at most {maximum} character{}",
                    if maximum == 1 { "" } else { "s" }
                ),
            );
            return None;
        }
        Some(Value::String(text.clone()))
    }

    fn integer(&mut self, strict: bool, input: &Json, path: &[PathSegment]) -> Option<i64> {
        match input {
            Json::Number(number) => {
                if let Some(value) = number.as_i64() {
                    return Some(value);
                }
                if let Some(value) = number.as_u64() {
                    return i64::try_from(value).ok().or_else(|| {
                        self.fail(path, "int_type", "Input should be a valid integer");
                        None
                    });
                }
                if strict {
                    self.fail(path, "int_type", "Input should be a valid integer");
                    return None;
                }
                let value = number.as_f64().unwrap_or(f64::NAN);
                self.integral(value, path)
            }
            Json::Bool(value) if !strict => Some(i64::from(*value)),
            Json::String(text) if !strict => {
                let trimmed = text.trim();
                if let Ok(value) = trimmed.parse::<i64>() {
                    return Some(value);
                }
                match trimmed.parse::<f64>() {
                    Ok(value) if value.is_finite() && value.fract() == 0.0 => {
                        self.integral(value, path)
                    }
                    _ => {
                        self.fail(
                            path,
                            "int_parsing",
                            "Input should be a valid integer, unable to parse string as an \
                             integer",
                        );
                        None
                    }
                }
            }
            _ => {
                self.fail(path, "int_type", "Input should be a valid integer");
                None
            }
        }
    }

    fn integral(&mut self, value: f64, path: &[PathSegment]) -> Option<i64> {
        if !value.is_finite() {
            self.fail(path, "finite_number", "Input should be a finite number");
            return None;
        }
        if value.fract() != 0.0 {
            self.fail(
                path,
                "int_from_float",
                "Input should be a valid integer, got a number with a fractional part",
            );
            return None;
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the value is finite and integral; out-of-range values saturate"
        )]
        Some(value as i64)
    }

    fn float(&mut self, strict: bool, input: &Json, path: &[PathSegment]) -> Option<f64> {
        let value = match input {
            Json::Number(number) => number.as_f64(),
            Json::Bool(value) if !strict => Some(if *value { 1.0 } else { 0.0 }),
            Json::String(text) if !strict => match python_float(text.trim()) {
                Some(value) => Some(value),
                None => {
                    self.fail(
                        path,
                        "float_parsing",
                        "Input should be a valid number, unable to parse string as a number",
                    );
                    return None;
                }
            },
            _ => None,
        };
        let Some(value) = value else {
            self.fail(path, "float_type", "Input should be a valid number");
            return None;
        };
        if !value.is_finite() && self.finite.last().copied().unwrap_or(false) {
            self.fail(path, "finite_number", "Input should be a finite number");
            return None;
        }
        Some(value)
    }

    fn boolean(&mut self, strict: bool, input: &Json, path: &[PathSegment]) -> Option<bool> {
        const PARSING: &str = "Input should be a valid boolean, unable to interpret input";
        match input {
            Json::Bool(value) => Some(*value),
            // An integer other than 0 or 1 cannot be read as a boolean; a
            // float other than those two is not a boolean at all.
            Json::Number(number) if !strict => match (number.as_i64(), number.as_f64()) {
                (Some(0), _) => Some(false),
                (Some(1), _) => Some(true),
                (Some(_), _) => {
                    self.fail(path, "bool_parsing", PARSING);
                    None
                }
                (None, Some(0.0)) => Some(false),
                (None, Some(1.0)) => Some(true),
                _ => {
                    self.fail(path, "bool_type", "Input should be a valid boolean");
                    None
                }
            },
            Json::String(text) if !strict => match text.to_ascii_lowercase().as_str() {
                "0" | "off" | "f" | "false" | "n" | "no" => Some(false),
                "1" | "on" | "t" | "true" | "y" | "yes" => Some(true),
                _ => {
                    self.fail(path, "bool_parsing", PARSING);
                    None
                }
            },
            _ => {
                self.fail(path, "bool_type", "Input should be a valid boolean");
                None
            }
        }
    }

    fn bounds(
        &mut self,
        value: f64,
        ge: Option<f64>,
        gt: Option<f64>,
        le: Option<f64>,
        lt: Option<f64>,
        path: &[PathSegment],
    ) -> bool {
        if let Some(bound) = gt
            && value <= bound
        {
            self.fail(
                path,
                "greater_than",
                format!("Input should be greater than {}", number_text(bound)),
            );
            return false;
        }
        if let Some(bound) = ge
            && value < bound
        {
            self.fail(
                path,
                "greater_than_equal",
                format!(
                    "Input should be greater than or equal to {}",
                    number_text(bound)
                ),
            );
            return false;
        }
        if let Some(bound) = lt
            && value >= bound
        {
            self.fail(
                path,
                "less_than",
                format!("Input should be less than {}", number_text(bound)),
            );
            return false;
        }
        if let Some(bound) = le
            && value > bound
        {
            self.fail(
                path,
                "less_than_equal",
                format!(
                    "Input should be less than or equal to {}",
                    number_text(bound)
                ),
            );
            return false;
        }
        true
    }
}

/// Python's `float()` over a string pydantic already stripped.
fn python_float(text: &str) -> Option<f64> {
    match text.to_ascii_lowercase().as_str() {
        "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
        "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
        "nan" | "+nan" | "-nan" => Some(f64::NAN),
        _ => text
            .replace('_', "")
            .parse::<f64>()
            .ok()
            .filter(|_| !text.starts_with('_') && !text.ends_with('_') && !text.contains("__")),
    }
}

/// Python's `==` between two JSON values: `1`, `1.0` and `true` compare
/// equal, which is how a literal meets its input.
fn python_equal(left: &Value, right: &Value) -> bool {
    let numeric = |value: &Value| match value {
        Value::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
        Value::Number(number) => number.as_f64(),
        _ => None,
    };
    match (numeric(left), numeric(right)) {
        (Some(left), Some(right)) => left == right,
        _ => left == right,
    }
}

/// A bound as Python prints it: an integral bound without a fractional part.
fn number_text(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

/// The expected values a literal or enum error lists: `'a'`, `'a' or 'b'`,
/// `'a', 'b' or 'c'`.
fn expected_list(values: &[Value]) -> String {
    let rendered = values.iter().map(python_repr).collect::<Vec<_>>();
    match rendered.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
    }
}

fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::String(text) => format!("'{text}'"),
        other => other.to_string(),
    }
}

/// A tag as the union error quotes it, without Python's own quotes.
fn python_repr_bare(value: &Json) -> String {
    match value {
        Json::String(text) => text.clone(),
        Json::Null => "None".to_owned(),
        Json::Bool(true) => "True".to_owned(),
        Json::Bool(false) => "False".to_owned(),
        Json::Number(number) => number.to_string(),
        Json::Array(_) | Json::Object(_) => {
            let mut text = String::new();
            let _ = write!(text, "{}", value.clone().into_value());
            text
        }
    }
}

/// The validators the reference writes itself, by the name the schema records.
///
/// Each takes the value the schema already validated and answers it, possibly
/// rewritten, or the reason it is refused. The reasons are this port's own
/// sentences.
mod custom {
    use serde_json::Value;

    pub(super) fn run(function: &str, value: Value) -> Result<Value, String> {
        match function {
            // Reference `AgentConfig._reject_empty_cwd`.
            "AgentConfig._reject_empty_cwd" => match &value {
                Value::String(text) if text.is_empty() => {
                    Err("a session directory cannot be empty".to_owned())
                }
                _ => Ok(value),
            },
            // Reference `UserDisplayContent.strip_nonempty`.
            "UserDisplayContent.strip_nonempty" => match value {
                Value::String(text) => {
                    let stripped = text.trim();
                    if stripped.is_empty() {
                        Err("the value cannot be blank".to_owned())
                    } else {
                        Ok(Value::String(stripped.to_owned()))
                    }
                }
                other => Ok(other),
            },
            // Reference `SessionSettingsUpdateParams.require_update`.
            "SessionSettingsUpdateParams.require_update" => {
                let set = |key: &str| value.get(key).is_some_and(|value| !value.is_null());
                if set("maxTurns") || set("maxTokens") {
                    Ok(value)
                } else {
                    Err("name at least one setting to update".to_owned())
                }
            }
            // Reference `SessionShellCommandParams.validate_action`.
            "SessionShellCommandParams.validate_action" => {
                let action = value.get("action").and_then(Value::as_str).unwrap_or("run");
                let command_blank = value
                    .get("command")
                    .and_then(Value::as_str)
                    .is_none_or(|command| command.trim().is_empty());
                let operation_missing = value.get("operationId").is_none_or(Value::is_null);
                if action == "run" && command_blank {
                    Err("running a shell command needs a command".to_owned())
                } else if action == "interrupt" && operation_missing {
                    Err("interrupting a shell command needs its operation".to_owned())
                } else {
                    Ok(value)
                }
            }
            // Reference `_TurnQueueInputParams.validate_entries`, through
            // `validate_turn_input_entries`.
            "_TurnQueueInputParams.validate_entries" => {
                let roles = value
                    .get("entries")
                    .and_then(Value::as_array)
                    .map(|entries| {
                        entries
                            .iter()
                            .map(|entry| {
                                entry
                                    .get("role")
                                    .and_then(Value::as_str)
                                    .unwrap_or("context")
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let users = roles.iter().filter(|role| **role == "user").count();
                if users > 1 {
                    Err("a turn takes at most one user entry".to_owned())
                } else if users == 1 && roles.last() != Some(&"user") {
                    Err("the user entry has to come last".to_owned())
                } else {
                    Ok(value)
                }
            }
            // Reference `SessionEmbeddedResourceContentBlock.validate_content`.
            "SessionEmbeddedResourceContentBlock.validate_content" => {
                let present = |key: &str| value.get(key).is_some_and(|value| !value.is_null());
                if present("text") == present("blob") {
                    Err("an embedded resource carries exactly one of text or blob".to_owned())
                } else {
                    Ok(value)
                }
            }
            _ => Ok(value),
        }
    }

    /// The validator names this module implements, which the schema has to
    /// stay within.
    #[cfg(test)]
    pub(super) const KNOWN: &[&str] = &[
        "AgentConfig._reject_empty_cwd",
        "SessionEmbeddedResourceContentBlock.validate_content",
        "SessionSettingsUpdateParams.require_update",
        "SessionShellCommandParams.validate_action",
        "UserDisplayContent.strip_nonempty",
        "_TurnQueueInputParams.validate_entries",
    ];
}

/// The validator names the compiled-in schema records.
#[cfg(test)]
fn schema_functions() -> &'static [String] {
    &SCHEMA.functions
}

#[cfg(test)]
mod wire_validation_tests;

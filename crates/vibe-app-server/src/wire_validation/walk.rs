//! The walk of an input over a schema node, which collects every issue
//! pydantic-core would and builds the value it would produce.

use serde_json::{Number, Value};
use vibe_core::pycompat::{self, pydantic};
use vibe_protocol::PathSegment;

use super::schema::{Extra, Node, SCHEMA};
use super::{Issue, custom};

#[derive(Default)]
pub(super) struct Walk {
    pub(super) issues: Vec<Issue>,
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

    /// The scalar a coercion read, or nothing once its refusal is recorded.
    fn read<T>(&mut self, read: Result<T, pydantic::Refusal>, path: &[PathSegment]) -> Option<T> {
        match read {
            Ok(value) => Some(value),
            Err(refusal) => {
                self.fail(path, refusal.kind, refusal.message);
                None
            }
        }
    }

    pub(super) fn model(
        &mut self,
        name: &str,
        input: &Value,
        path: &mut Vec<PathSegment>,
        from_attributes: bool,
    ) -> Option<Value> {
        let Some(model) = SCHEMA.defs.get(name) else {
            self.fail(path, "model_type", format!("Unknown model {name}"));
            return None;
        };
        let Value::Object(entries) = input else {
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
        // A key no field consumed is extra: with `AliasChoices`, the second of
        // two spellings given together is one.
        let mut consumed = Vec::new();
        for field in &model.fields {
            let found = field
                .keys()
                .iter()
                .find_map(|key| input.get(key).map(|value| (key, value)));
            let key = found.map_or(&field.alias, |(key, _)| key);
            path.push(PathSegment::Field(key.clone()));
            match found {
                Some((key, value)) => {
                    consumed.push(key);
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
            if consumed.contains(&key) {
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
                    output.insert(key.clone(), value.clone());
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

    pub(super) fn node(
        &mut self,
        node: &Node,
        input: &Value,
        path: &mut Vec<PathSegment>,
    ) -> Option<Value> {
        match node {
            Node::Ref { name } => self.model(name, input, path, false),
            Node::Default { schema } => self.node(schema, input, path),
            Node::Nullable { schema } => match input {
                Value::Null => Some(Value::Null),
                _ => self.node(schema, input, path),
            },
            Node::FunctionBefore { function, schema } => {
                let prepared = custom::before(function, input);
                self.node(schema, &prepared, path)
            }
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
                if expected
                    .iter()
                    .any(|candidate| pycompat::python_eq(candidate, input))
                {
                    Some(input.clone())
                } else {
                    self.fail(
                        path,
                        "literal_error",
                        format!("Input should be {}", pydantic::expected_literals(expected)),
                    );
                    None
                }
            }
            Node::Enum { values } => {
                if values.iter().any(|candidate| candidate == input) {
                    Some(input.clone())
                } else {
                    self.fail(
                        path,
                        "enum",
                        format!("Input should be {}", pydantic::expected_literals(values)),
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
                let value = self.read(pydantic::integer(input, *strict), path)?;
                // Bounds compare the integer exactly while it fits a float's
                // mantissa, which every bound the reference declares does.
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "bounds are small integers the reference declares"
                )]
                let reading = value as f64;
                // Past the 64-bit range the value saturates, as a float does.
                let saturated =
                    i64::try_from(value).unwrap_or(if value < 0 { i64::MIN } else { i64::MAX });
                self.bounds(reading, *ge, *gt, *le, *lt, path)
                    .then(|| Value::Number(saturated.into()))
            }
            Node::Float {
                strict,
                ge,
                gt,
                le,
                lt,
            } => {
                let value = self.read(pydantic::float(input, *strict), path)?;
                if !value.is_finite() && self.finite.last().copied().unwrap_or(false) {
                    self.fail(path, "finite_number", "Input should be a finite number");
                    return None;
                }
                if !self.bounds(value, *ge, *gt, *le, *lt, path) {
                    return None;
                }
                Some(Number::from_f64(value).map_or(Value::Null, Value::Number))
            }
            Node::Bool { strict } => self
                .read(pydantic::boolean(input, *strict), path)
                .map(Value::Bool),
            Node::None => match input {
                Value::Null => Some(Value::Null),
                _ => {
                    self.fail(path, "none_required", "Input should be None");
                    None
                }
            },
            Node::Any | Node::JsonValue => Some(input.clone()),
        }
    }

    fn list(
        &mut self,
        items: &Node,
        min_length: Option<usize>,
        max_length: Option<usize>,
        input: &Value,
        path: &mut Vec<PathSegment>,
    ) -> Option<Value> {
        let Value::Array(entries) = input else {
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
        input: &Value,
        path: &mut Vec<PathSegment>,
    ) -> Option<Value> {
        let Value::Object(entries) = input else {
            self.fail(path, "dict_type", "Input should be a valid dictionary");
            return None;
        };
        let before = self.issues.len();
        let mut output = serde_json::Map::new();
        for (key, value) in entries {
            path.push(PathSegment::Field(key.clone()));
            path.push(PathSegment::Field("[key]".to_owned()));
            let validated_key = self.node(keys, &Value::String(key.clone()), path);
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
        input: &Value,
        path: &mut Vec<PathSegment>,
    ) -> Option<Value> {
        if !matches!(input, Value::Object(_)) {
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
            Value::String(tag) => choices
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
                    pycompat::str_json(tag)
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
        input: &Value,
        path: &mut [PathSegment],
    ) -> Option<Value> {
        let _ = strict;
        let Value::String(text) = input else {
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

/// A bound as Python prints it: an integral bound without a fractional part.
fn number_text(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

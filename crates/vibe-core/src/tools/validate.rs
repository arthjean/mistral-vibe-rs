//! Checking one call's arguments against the tool's declared schema.
//!
//! The reference validates with Pydantic, which does more than reject a wrong
//! type: it coerces the spellings Python accepts, fills declared defaults, and
//! reports every violation it found, each by its path. A model that sends `"3"`
//! where an integer is declared is answered the same way on both sides only if the same
//! coercions run here, which is why this is a coercion pass followed by a
//! validation pass rather than a single type check.
//!
//! Both passes are bounded by [`MAX_SCHEMA_DEPTH`]: a schema is data a remote
//! server can supply, so a cyclic `$ref` must cost a diagnostic rather than the
//! stack.

use serde_json::{Map, Value};

use super::{ToolError, resolve_reference};

/// One place a call's arguments failed the tool's declared schema.
///
/// Validation reports every one of them rather than the first, because the
/// reference validates with Pydantic and Pydantic collects: a call breaking
/// three arguments is answered by naming three, and a model that reads back
/// only the first has to guess at the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaViolation {
    /// Where the value sits in the argument object, in the `$.field[0].sub`
    /// spelling.
    pub path: String,
    /// What the schema wanted there.
    pub message: String,
}

impl std::fmt::Display for SchemaViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.path, self.message)
    }
}

/// Renders a violation list into the one line a rejection carries.
pub(super) fn render_violations(violations: &[SchemaViolation]) -> String {
    violations
        .iter()
        .map(SchemaViolation::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// A single violation in the shape the recursion passes around.
fn violation(path: &str, message: impl Into<String>) -> Vec<SchemaViolation> {
    vec![SchemaViolation {
        path: path.to_owned(),
        message: message.into(),
    }]
}

/// Turns an accumulated list into the verdict for one subtree.
fn verdict(violations: Vec<SchemaViolation>) -> Result<(), Vec<SchemaViolation>> {
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// Bounds `$ref` resolution and nesting so a cyclic schema fails instead of
/// recursing until the stack gives out.
const MAX_SCHEMA_DEPTH: usize = 32;

/// Validates tool arguments with the semantics the Python reference gets from
/// Pydantic: `$ref` resolved against `$defs`, `anyOf` accepted when any variant
/// matches, `items` applied element by element, array-form `type` accepted, and
/// unknown properties tolerated unless the schema forbids them.
pub fn validate_arguments(arguments: &Value, schema: &Value) -> Result<(), Vec<SchemaViolation>> {
    validate_at(arguments, schema, schema, "$", 0)
}

/// Fills every absent property that declares a `default`, so a handler reads the
/// same value the reference model would have materialized.
pub fn apply_defaults(arguments: &mut Value, schema: &Value) {
    fill_defaults(arguments, schema, schema, 0);
}

/// Rewrites scalars the reference model would have accepted in a looser form,
/// then validates what the handler is actually going to read.
///
/// The reference builds its arguments through Pydantic in lax mode, which
/// coerces before it validates: `"yes"` reaches a `bool` field as `true` and
/// `"17"` reaches an `int` field as `17`. Validating the raw payload therefore
/// rejects calls the reference accepts, which is what made two of the 92
/// argument fixtures diverge. Coercing first closes that gap and, because the
/// rewrite is in place, the handler reads the coerced value rather than the
/// string the model sent.
///
/// This is the single entry point for both: `ToolRegistry::invoke_stream` calls
/// it before dispatch, and the fixture replay calls it to reproduce a verdict.
///
/// The rejection names `tool` because the reference names it: upstream raises
/// from the tool wrapper, so the model reads which tool refused the call rather
/// than only where the payload was wrong.
pub fn coerce_and_validate(
    tool: &str,
    arguments: &mut Value,
    schema: &Value,
) -> Result<(), ToolError> {
    coerce_at(arguments, schema, schema, 0);
    validate_arguments(arguments, schema).map_err(|violations| ToolError::InvalidArguments {
        tool: tool.to_owned(),
        violations,
    })
}

/// Applies the reference scalar coercion in place, leaving anything it cannot
/// coerce untouched so validation reports it against the property that declared
/// the type.
fn coerce_at(value: &mut Value, schema: &Value, root: &Value, depth: usize) {
    if depth > MAX_SCHEMA_DEPTH {
        return;
    }
    let Some(object) = schema.as_object() else {
        return;
    };
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        if let Some(target) = resolve_reference(reference, root) {
            coerce_at(value, target, root, depth + 1);
        }
        return;
    }
    if let Some(variants) = object.get("anyOf").and_then(Value::as_array) {
        coerce_union(value, variants, root, depth);
        return;
    }
    match object.get("type") {
        Some(Value::String(declared)) => coerce_declared(value, declared, object, root, depth),
        // The array form is a union spelled inline, so it follows the same
        // exact-match-first rule.
        Some(Value::Array(declared)) => {
            let variants = declared
                .iter()
                .filter_map(Value::as_str)
                .map(|name| serde_json::json!({ "type": name }))
                .collect::<Vec<_>>();
            coerce_union(value, &variants, root, depth);
        }
        _ => {}
    }
}

/// Picks the branch of a union the way Pydantic's smart mode does: a variant the
/// value already satisfies wins over any coercion, and only when none matches is
/// each variant tried in declaration order.
fn coerce_union(value: &mut Value, variants: &[Value], root: &Value, depth: usize) {
    if variants
        .iter()
        .any(|variant| validate_at(value, variant, root, "$", depth + 1).is_ok())
    {
        return;
    }
    for variant in variants {
        let mut candidate = value.clone();
        coerce_at(&mut candidate, variant, root, depth + 1);
        if validate_at(&candidate, variant, root, "$", depth + 1).is_ok() {
            *value = candidate;
            return;
        }
    }
}

/// Coerces `value` toward a single declared type, recursing into containers.
fn coerce_declared(
    value: &mut Value,
    declared: &str,
    schema: &Map<String, Value>,
    root: &Value,
    depth: usize,
) {
    match declared {
        "boolean" => {
            if let Some(coerced) = coerce_boolean(value) {
                *value = coerced;
            }
        }
        "integer" => {
            if let Some(coerced) = coerce_integer(value) {
                *value = coerced;
            }
        }
        "number" => {
            if let Some(coerced) = coerce_number(value) {
                *value = coerced;
            }
        }
        // A `string` field coerces nothing: the reference accepts only `str`
        // and `bytes`, so a number or a boolean stays as it is and validation
        // rejects it naming the property.
        "array" => {
            if let Some(items) = value.as_array_mut()
                && let Some(item_schema) = schema.get("items")
            {
                for item in items {
                    coerce_at(item, item_schema, root, depth + 1);
                }
            }
        }
        "object" => {
            if let Some(fields) = value.as_object_mut()
                && let Some(properties) = schema.get("properties").and_then(Value::as_object)
            {
                for (name, field_schema) in properties {
                    if let Some(field) = fields.get_mut(name) {
                        coerce_at(field, field_schema, root, depth + 1);
                    }
                }
            }
        }
        _ => {}
    }
}

/// The booleanish forms the reference accepts, or [`None`] when the value is
/// already a boolean or cannot become one.
///
/// The word set is case-insensitive and tolerates no surrounding whitespace,
/// both measured against the reference interpreter.
fn coerce_boolean(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) => {
            let lowered = text.to_ascii_lowercase();
            match lowered.as_str() {
                "yes" | "on" | "true" | "t" | "y" | "1" => Some(Value::Bool(true)),
                "no" | "off" | "false" | "f" | "n" | "0" => Some(Value::Bool(false)),
                _ => None,
            }
        }
        // Only the two values that name a truth: `2` and `-1` are rejected
        // rather than treated as truthy.
        Value::Number(number) => {
            let flag = if let Some(integer) = number.as_i64() {
                match integer {
                    0 => false,
                    1 => true,
                    _ => return None,
                }
            } else {
                let float = number.as_f64()?;
                if float == 0.0 {
                    false
                } else if float == 1.0 {
                    true
                } else {
                    return None;
                }
            };
            Some(Value::Bool(flag))
        }
        _ => None,
    }
}

/// The integral forms the reference accepts: a boolean, a float whose fraction
/// is zero, and a string spelling either of those.
fn coerce_integer(value: &Value) -> Option<Value> {
    match value {
        Value::Bool(flag) => Some(Value::from(i64::from(*flag))),
        Value::Number(number) => {
            if number.is_i64() || number.is_u64() {
                return None;
            }
            let float = number.as_f64()?;
            (float.fract() == 0.0).then(|| Value::from(float as i64))
        }
        Value::String(text) => parse_integer(text).map(Value::from),
        _ => None,
    }
}

/// The numeric forms the reference accepts for a float field. An integer is
/// already a valid JSON number, so it is left alone rather than rewritten into
/// a float that no acceptance decision depends on.
fn coerce_number(value: &Value) -> Option<Value> {
    match value {
        Value::Bool(flag) => Some(Value::from(f64::from(u8::from(*flag)))),
        Value::String(text) => {
            let normalized = text.trim().replace('_', "");
            let parsed = normalized.parse::<f64>().ok()?;
            // JSON carries no infinity and no NaN, so a string spelling either
            // cannot be represented and is refused instead of silently clamped.
            parsed.is_finite().then(|| Value::from(parsed))
        }
        _ => None,
    }
}

/// Parses the integer spellings the reference accepts and no others.
///
/// Python's `int()` trims whitespace, honors a sign and ignores underscore
/// separators, and Pydantic additionally accepts a decimal spelling whose
/// fraction is zero. It refuses exponent notation, so `"1e2"` is not an integer
/// even though it is a whole number.
fn parse_integer(text: &str) -> Option<i64> {
    let normalized = text.trim().replace('_', "");
    if normalized.is_empty() {
        return None;
    }
    if let Ok(integer) = normalized.parse::<i64>() {
        return Some(integer);
    }
    let decimal = normalized
        .split_once('.')
        .filter(|_| !normalized.contains(['e', 'E']))?;
    let whole = format!("{}{}", decimal.0, decimal.1);
    if !whole
        .trim_start_matches(['+', '-'])
        .chars()
        .all(|character| character.is_ascii_digit())
    {
        return None;
    }
    let parsed = normalized.parse::<f64>().ok()?;
    (parsed.fract() == 0.0 && parsed.is_finite()).then_some(parsed as i64)
}

/// Collects every violation the value carries against the schema.
///
/// The scalar checks short-circuit, because a value of the wrong type has
/// nothing left to say, but the container checks accumulate: a missing
/// property, an extra one and a mismatched sibling are three answers to one
/// call and all three are reported.
pub(super) fn validate_at(
    value: &Value,
    schema: &Value,
    root: &Value,
    path: &str,
    depth: usize,
) -> Result<(), Vec<SchemaViolation>> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(violation(
            path,
            format!("schema nesting exceeds the {MAX_SCHEMA_DEPTH}-level bound"),
        ));
    }
    let schema = schema
        .as_object()
        .ok_or_else(|| violation(path, "schema is not an object"))?;
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let target = resolve_reference(reference, root)
            .ok_or_else(|| violation(path, format!("unresolved $ref `{reference}`")))?;
        return validate_at(value, target, root, path, depth + 1);
    }
    if let Some(variants) = schema.get("anyOf").and_then(Value::as_array) {
        let mut matched = false;
        let mut failures = Vec::new();
        for variant in variants {
            match validate_at(value, variant, root, path, depth + 1) {
                Ok(()) => {
                    matched = true;
                    break;
                }
                Err(error) => failures.push((variant, error)),
            }
        }
        if !matched {
            // A nullable property is one variant plus a null branch, so when
            // the value carries the declared type its own diagnosis is the
            // useful one; a value matching no variant gets the generic error.
            return Err(failures
                .into_iter()
                .find(|(variant, _)| declares_type_of(variant, value, root))
                .map(|(_, error)| error)
                .unwrap_or_else(|| violation(path, "value matches no declared variant")));
        }
    }
    match schema.get("type") {
        Some(Value::String(expected)) => validate_type(value, expected, path)?,
        Some(Value::Array(expected)) => {
            let matched = expected
                .iter()
                .filter_map(Value::as_str)
                .any(|expected| validate_type(value, expected, path).is_ok());
            if !matched {
                return Err(violation(path, "value matches no declared type"));
            }
        }
        _ => {}
    }
    if let Some(variants) = schema.get("enum").and_then(Value::as_array)
        && !variants.contains(value)
    {
        return Err(violation(path, "value is not in enum"));
    }
    validate_bounds(value, schema, path)?;
    let mut violations = Vec::new();
    match value {
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    if let Err(found) = validate_at(
                        item,
                        item_schema,
                        root,
                        &format!("{path}[{index}]"),
                        depth + 1,
                    ) {
                        violations.extend(found);
                    }
                }
            }
        }
        Value::Object(object) => {
            let empty = Map::new();
            let properties = schema
                .get("properties")
                .and_then(Value::as_object)
                .unwrap_or(&empty);
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for field in required.iter().filter_map(Value::as_str) {
                    if !object.contains_key(field) {
                        violations.extend(violation(
                            &format!("{path}.{field}"),
                            "required property is missing",
                        ));
                    }
                }
            }
            if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false) {
                for field in object
                    .keys()
                    .filter(|field| !properties.contains_key(*field))
                {
                    violations.extend(violation(
                        &format!("{path}.{field}"),
                        "additional property is not allowed",
                    ));
                }
            }
            for (field, field_schema) in properties {
                if let Some(field_value) = object.get(field)
                    && let Err(found) = validate_at(
                        field_value,
                        field_schema,
                        root,
                        &format!("{path}.{field}"),
                        depth + 1,
                    )
                {
                    violations.extend(found);
                }
            }
        }
        _ => {}
    }
    verdict(violations)
}

/// True when `schema` declares the JSON type `value` carries, which is how an
/// `anyOf` branch is matched to the value it was meant to describe.
fn declares_type_of(schema: &Value, value: &Value, root: &Value) -> bool {
    let Some(schema) = schema.as_object() else {
        return false;
    };
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        return resolve_reference(reference, root)
            .is_some_and(|target| declares_type_of(target, value, root));
    }
    let declared = |name: &str| validate_type(value, name, "$").is_ok();
    match schema.get("type") {
        Some(Value::String(name)) => declared(name),
        Some(Value::Array(names)) => names.iter().filter_map(Value::as_str).any(declared),
        _ => false,
    }
}

fn validate_bounds(
    value: &Value,
    schema: &Map<String, Value>,
    path: &str,
) -> Result<(), Vec<SchemaViolation>> {
    let violation = |message: String| violation(path, message);
    if let Some(number) = value.as_f64() {
        for keyword in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
            let Some(bound) = schema.get(keyword).and_then(Value::as_f64) else {
                continue;
            };
            let satisfied = match keyword {
                "minimum" => number >= bound,
                "maximum" => number <= bound,
                "exclusiveMinimum" => number > bound,
                _ => number < bound,
            };
            if !satisfied {
                return Err(violation(format!("value violates {keyword} {bound}")));
            }
        }
    }
    if let Some(text) = value.as_str() {
        let length = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
        if let Some(bound) = schema.get("minLength").and_then(Value::as_u64)
            && length < bound
        {
            return Err(violation(format!(
                "value is shorter than minLength {bound}"
            )));
        }
        if let Some(bound) = schema.get("maxLength").and_then(Value::as_u64)
            && length > bound
        {
            return Err(violation(format!("value is longer than maxLength {bound}")));
        }
    }
    if let Some(items) = value.as_array() {
        let length = u64::try_from(items.len()).unwrap_or(u64::MAX);
        if let Some(bound) = schema.get("minItems").and_then(Value::as_u64)
            && length < bound
        {
            return Err(violation(format!(
                "array holds fewer than minItems {bound}"
            )));
        }
        if let Some(bound) = schema.get("maxItems").and_then(Value::as_u64)
            && length > bound
        {
            return Err(violation(format!("array holds more than maxItems {bound}")));
        }
    }
    Ok(())
}

fn fill_defaults(value: &mut Value, schema: &Value, root: &Value, depth: usize) {
    if depth > MAX_SCHEMA_DEPTH {
        return;
    }
    let Some(schema) = schema.as_object() else {
        return;
    };
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        if let Some(target) = resolve_reference(reference, root) {
            fill_defaults(value, target, root, depth + 1);
        }
        return;
    }
    if let Some(variants) = schema.get("anyOf").and_then(Value::as_array) {
        if let Some(variant) = variants
            .iter()
            .find(|variant| declares_type_of(variant, value, root))
        {
            fill_defaults(value, variant, root, depth + 1);
        }
        return;
    }
    match value {
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for item in items {
                    fill_defaults(item, item_schema, root, depth + 1);
                }
            }
        }
        Value::Object(object) => {
            let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
                return;
            };
            for (field, field_schema) in properties {
                match object.get_mut(field) {
                    Some(present) => fill_defaults(present, field_schema, root, depth + 1),
                    None => {
                        if let Some(default) = field_schema.get("default") {
                            object.insert(field.clone(), default.clone());
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn validate_type(value: &Value, expected: &str, path: &str) -> Result<(), Vec<SchemaViolation>> {
    let valid = match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(violation(path, format!("expected {expected}")))
    }
}

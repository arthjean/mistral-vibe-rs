//! The reference scalar coercion, measured rather than assumed.
//!
//! Every expectation below was taken from the reference interpreter on
//! 2026-08-06 (Pydantic 2.13.4) by validating a one-field model against each
//! value and recording the verdict and the coerced result. The reference builds
//! tool arguments through Pydantic in lax mode, so a payload the model sent as
//! `"yes"` reaches the handler as `true`; reproducing that is what US-100 asks
//! for.

use std::sync::Arc;

use serde_json::{Value, json};

use super::{
    OwnedToolHandlerFuture, ToolAvailability, ToolError, ToolExecutionOutput, ToolHandler,
    ToolInvocation, ToolOutputSink, ToolPresentationKind, ToolRegistry, ToolSource, ToolSpec,
    coerce_and_validate,
};

/// A single optional property of the declared type, which is the shape every
/// published tool uses for its scalars.
fn schema_of(declared: &str) -> Value {
    json!({
        "type": "object",
        "properties": { "v": { "type": declared } },
    })
}

/// The nullable form, spelled the way the reference emits it.
fn nullable_schema_of(declared: &str) -> Value {
    json!({
        "type": "object",
        "properties": {
            "v": { "anyOf": [{ "type": declared }, { "type": "null" }] },
        },
    })
}

/// The value a handler would receive, or the rejection validation produced.
fn coerce(schema: &Value, value: Value) -> Result<Value, ToolError> {
    let mut arguments = json!({ "v": value });
    coerce_and_validate(&mut arguments, schema)?;
    Ok(arguments
        .get("v")
        .cloned()
        .expect("the property survives coercion"))
}

fn accepts(schema: &Value, value: Value, expected: Value) {
    let raw = value.clone();
    match coerce(schema, value) {
        Ok(coerced) => assert_eq!(
            coerced, expected,
            "the reference coerces {raw} to {expected}, this port produced {coerced}"
        ),
        Err(error) => panic!("the reference accepts {raw}, this port rejected it: {error}"),
    }
}

fn rejects(schema: &Value, value: Value) {
    let raw = value.clone();
    if let Ok(coerced) = coerce(schema, value) {
        panic!("the reference rejects {raw}, this port accepted it as {coerced}");
    }
}

#[test]
fn a_boolean_accepts_every_booleanish_word_in_any_case() {
    let schema = schema_of("boolean");
    for word in ["yes", "on", "true", "t", "y", "1"] {
        accepts(&schema, json!(word), json!(true));
        accepts(&schema, json!(word.to_uppercase()), json!(true));
    }
    for word in ["no", "off", "false", "f", "n", "0"] {
        accepts(&schema, json!(word), json!(false));
        accepts(&schema, json!(word.to_uppercase()), json!(false));
    }
    // Mixed case, since the reference lowercases rather than matching exactly.
    accepts(&schema, json!("YeS"), json!(true));
    accepts(&schema, json!("Off"), json!(false));
}

#[test]
fn a_boolean_accepts_the_two_numbers_that_name_a_truth() {
    let schema = schema_of("boolean");
    accepts(&schema, json!(1), json!(true));
    accepts(&schema, json!(0), json!(false));
    accepts(&schema, json!(1.0), json!(true));
    accepts(&schema, json!(0.0), json!(false));
    accepts(&schema, json!(true), json!(true));
    accepts(&schema, json!(false), json!(false));
}

#[test]
fn a_boolean_rejects_what_the_reference_rejects() {
    let schema = schema_of("boolean");
    for value in [
        json!(2),
        json!(-1),
        json!(1.5),
        json!(""),
        json!("maybe"),
        json!("2"),
        // A booleanish word is matched exactly: the reference tolerates no
        // surrounding whitespace, and `"1.0"` is not one of its words even
        // though the float `1.0` is accepted.
        json!(" yes "),
        json!("yes\n"),
        json!("1.0"),
        json!(null),
        json!([]),
        json!({}),
    ] {
        rejects(&schema, value);
    }
}

#[test]
fn a_string_coerces_nothing() {
    let schema = schema_of("string");
    accepts(&schema, json!("17"), json!("17"));
    accepts(&schema, json!(""), json!(""));
    // The reference accepts only `str` and `bytes`, so a number or a boolean is
    // a rejection rather than a value to render.
    for value in [json!(17), json!(17.5), json!(true), json!(null), json!([])] {
        rejects(&schema, value);
    }
}

#[test]
fn an_integer_accepts_the_integral_spellings() {
    let schema = schema_of("integer");
    accepts(&schema, json!(17), json!(17));
    accepts(&schema, json!("17"), json!(17));
    accepts(&schema, json!("-17"), json!(-17));
    accepts(&schema, json!("+17"), json!(17));
    accepts(&schema, json!(" 17 "), json!(17));
    accepts(&schema, json!("017"), json!(17));
    accepts(&schema, json!("1_000"), json!(1000));
    // A float whose fraction is zero, written either way.
    accepts(&schema, json!(17.0), json!(17));
    accepts(&schema, json!("17.0"), json!(17));
    accepts(&schema, json!("17.00"), json!(17));
    // A boolean is an integer to the reference.
    accepts(&schema, json!(true), json!(1));
    accepts(&schema, json!(false), json!(0));
}

#[test]
fn an_integer_rejects_a_fraction_and_an_exponent() {
    let schema = schema_of("integer");
    for value in [
        json!(17.5),
        json!("17.5"),
        json!(""),
        json!("seventeen"),
        // Whole but written as an exponent, which the reference refuses for an
        // integer field even though it accepts it for a float one.
        json!("1e2"),
        json!("0x10"),
        json!("inf"),
        json!("nan"),
        json!(null),
    ] {
        rejects(&schema, value);
    }
}

#[test]
fn a_number_accepts_the_numeric_spellings() {
    let schema = schema_of("number");
    accepts(&schema, json!("1.5"), json!(1.5));
    accepts(&schema, json!(" 1.5 "), json!(1.5));
    accepts(&schema, json!("+1.5"), json!(1.5));
    accepts(&schema, json!("1e3"), json!(1000.0));
    accepts(&schema, json!(".5"), json!(0.5));
    accepts(&schema, json!(true), json!(1.0));
    accepts(&schema, json!(false), json!(0.0));
    // An integer is already a valid JSON number, so it reaches the handler
    // unchanged rather than being rewritten into a float.
    accepts(&schema, json!(17), json!(17));
    accepts(&schema, json!(1.5), json!(1.5));
}

#[test]
fn a_number_rejects_what_json_cannot_carry() {
    let schema = schema_of("number");
    for value in [
        json!("seventeen"),
        json!(""),
        json!(null),
        // JSON has no infinity and no NaN, so the reference spellings have no
        // representable target here and are refused rather than clamped.
        json!("inf"),
        json!("-inf"),
        json!("nan"),
    ] {
        rejects(&schema, value);
    }
}

#[test]
fn a_nullable_property_coerces_through_its_concrete_branch() {
    let boolean = nullable_schema_of("boolean");
    accepts(&boolean, json!("yes"), json!(true));
    accepts(&boolean, json!(null), json!(null));
    rejects(&boolean, json!("maybe"));

    let integer = nullable_schema_of("integer");
    accepts(&integer, json!("17"), json!(17));
    accepts(&integer, json!(null), json!(null));
    rejects(&integer, json!("yes"));
}

#[test]
fn a_union_prefers_the_branch_the_value_already_satisfies() {
    // Measured against `Union[str, int]`: `"17"` stays a string because a
    // variant matches it exactly, and coercion is only reached when none does.
    let schema = json!({
        "type": "object",
        "properties": {
            "v": { "anyOf": [{ "type": "string" }, { "type": "integer" }] },
        },
    });
    accepts(&schema, json!("17"), json!("17"));
    accepts(&schema, json!(17), json!(17));
    accepts(&schema, json!(true), json!(1));
}

#[test]
fn coercion_reaches_a_nested_property_and_an_array_item() {
    let schema = json!({
        "type": "object",
        "properties": {
            "nested": {
                "type": "object",
                "properties": { "flag": { "type": "boolean" } },
            },
            "items": {
                "type": "array",
                "items": { "type": "integer" },
            },
        },
    });
    let mut arguments = json!({
        "nested": { "flag": "on" },
        "items": ["1", 2.0, true],
    });
    coerce_and_validate(&mut arguments, &schema).expect("the reference accepts this payload");
    assert_eq!(
        arguments,
        json!({ "nested": { "flag": true }, "items": [1, 2, 1] })
    );
}

#[test]
fn a_rejection_names_the_property_path() {
    let schema = schema_of("boolean");
    let error = coerce(&schema, json!("maybe")).expect_err("an unrecognized word is rejected");
    match error {
        ToolError::SchemaViolation { path, .. } => assert_eq!(path, "$.v"),
        other => panic!("expected a schema violation, got {other}"),
    }
}

/// Publishes the argument payload the handler actually received, so a test can
/// assert what a tool reads rather than what the registry was handed.
fn echo_handler() -> Arc<dyn ToolHandler> {
    Arc::new(
        move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let arguments = invocation.arguments.clone();
            Box::pin(async move { Ok(ToolExecutionOutput::new(String::new()).typed(arguments)) })
        },
    )
}

/// The arguments a handler reads when the registry dispatches `arguments`, taken
/// from the real entry point rather than from the coercion helper directly.
async fn arguments_the_handler_received(input_schema: Value, arguments: Value) -> Value {
    let registry = ToolRegistry::default();
    registry
        .register(
            ToolSpec {
                name: "coerced".to_owned(),
                description: "Echoes what it received".to_owned(),
                input_schema,
                output_schema: None,
                config: Value::Null,
                state: Value::Null,
                availability: ToolAvailability::Available,
                presentation: ToolPresentationKind::Generic,
                source: ToolSource::BuiltIn,
                selection_priority: 0,
            },
            echo_handler(),
        )
        .expect("register");
    registry
        .invoke(
            "coerced",
            ToolInvocation {
                call_id: "call-1".to_owned(),
                arguments,
            },
        )
        .await
        .expect("the reference accepts this payload")
        .typed_result
}

#[tokio::test]
async fn a_handler_reads_the_coerced_value_and_not_the_raw_one() {
    let schema = json!({
        "type": "object",
        "properties": {
            "replace_all": { "type": "boolean", "default": false },
            "max_matches": { "anyOf": [{ "type": "integer" }, { "type": "null" }] },
            "path": { "type": "string" },
        },
        "required": ["path"],
    });

    let received = arguments_the_handler_received(
        schema,
        json!({ "path": "src", "replace_all": "yes", "max_matches": "50" }),
    )
    .await;

    assert_eq!(
        received,
        json!({ "path": "src", "replace_all": true, "max_matches": 50 }),
        "the handler must read the booleans and integers the reference materializes, not the \
         strings the model sent"
    );
}

#[tokio::test]
async fn coercion_runs_before_defaults_so_a_default_is_not_overwritten() {
    let schema = json!({
        "type": "object",
        "properties": {
            "flag": { "type": "boolean", "default": true },
            "count": { "type": "integer", "default": 10 },
        },
    });

    // An absent property still takes its declared default.
    let defaulted = arguments_the_handler_received(schema.clone(), json!({})).await;
    assert_eq!(defaulted, json!({ "flag": true, "count": 10 }));

    // A present one is coerced and keeps the sent value rather than the default.
    let sent = arguments_the_handler_received(schema, json!({ "flag": "off", "count": "3" })).await;
    assert_eq!(sent, json!({ "flag": false, "count": 3 }));
}

#[tokio::test]
async fn a_value_the_reference_rejects_never_reaches_the_handler() {
    let registry = ToolRegistry::default();
    registry
        .register(
            ToolSpec {
                name: "coerced".to_owned(),
                description: "Echoes what it received".to_owned(),
                input_schema: schema_of("boolean"),
                output_schema: None,
                config: Value::Null,
                state: Value::Null,
                availability: ToolAvailability::Available,
                presentation: ToolPresentationKind::Generic,
                source: ToolSource::BuiltIn,
                selection_priority: 0,
            },
            echo_handler(),
        )
        .expect("register");

    let error = registry
        .invoke(
            "coerced",
            ToolInvocation {
                call_id: "call-1".to_owned(),
                arguments: json!({ "v": "maybe" }),
            },
        )
        .await
        .expect_err("an unrecognized word is refused before dispatch");
    assert!(
        matches!(error, ToolError::SchemaViolation { .. }),
        "{error}"
    );
}

#[test]
fn an_uncoercible_value_is_left_alone_for_validation_to_report() {
    // Coercion must not consume the payload it cannot convert, otherwise the
    // diagnosis would name a value the caller never sent.
    let schema = schema_of("integer");
    let mut arguments = json!({ "v": "seventeen" });
    let error = coerce_and_validate(&mut arguments, &schema).expect_err("rejected");
    assert_eq!(arguments, json!({ "v": "seventeen" }), "{error}");
}

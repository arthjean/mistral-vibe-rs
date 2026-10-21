//! Registering, publishing and invoking tools through one registry.

use std::collections::BTreeSet;

use serde_json::json;

use super::*;
use crate::matching::NameFilter;
use crate::schema::{ObjectSchema, Property};

fn spec(priority: i32) -> ToolSpec {
    ToolSpec {
        name: "read".to_owned(),
        description: "Read a file".to_owned(),
        input_schema: ObjectSchema::new()
            .required("path", Property::string())
            .build(),
        output_schema: Some(
            ObjectSchema::new()
                .required("content", Property::string())
                .build(),
        ),
        config: json!({"maxBytes": 1024}),
        state: json!({"calls": 0}),
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Read,
        source: ToolSource::BuiltIn,
        selection_priority: priority,
    }
}

fn handler(content: &'static str) -> Arc<dyn ToolHandler> {
    Arc::new(
        move |_invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
            Box::pin(async move {
                Ok(ToolExecutionOutput {
                    skip: None,
                    turn_failure: None,
                    approval: None,
                    failure: None,
                    typed_result: json!({"content": content}),
                    model_text: content.to_owned(),
                    display: json!({"kind": "read"}),
                    projected_result: serde_json::Value::Null,
                    chunks: vec![content.to_owned()],
                    pending_injection: None,
                    remote_result: None,
                })
            })
        },
    )
}

/// Publishes the argument payload the handler actually received, so a test
/// can assert what defaults and pass-through keys reach a tool.
fn echo_handler() -> Arc<dyn ToolHandler> {
    Arc::new(
        move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
            let arguments = invocation.arguments.clone();
            Box::pin(async move { Ok(ToolExecutionOutput::new(String::new()).typed(arguments)) })
        },
    )
}

/// Remote providers publish `{alias}_{tool}`, a rule two providers can
/// land on for the same string. The second claim is rejected and both
/// sources are named, rather than one silently shadowing the other.
#[test]
fn a_second_source_claiming_a_published_name_is_rejected_naming_both() {
    let tools = ToolRegistry::default();
    tools
        .register_exclusive(
            spec(50),
            handler("first"),
            "MCP server `docs` tool `read`",
            None,
        )
        .expect("the first claim wins the name");

    let conflict = tools
        .register_exclusive(
            spec(50),
            handler("second"),
            "MCP server `docs_read` tool ``",
            None,
        )
        .expect_err("the second claim cannot take the name");

    let message = conflict.to_string();
    assert!(
        message.contains("MCP server `docs` tool `read`"),
        "{message}"
    );
    assert!(
        message.contains("MCP server `docs_read` tool ``"),
        "{message}"
    );
    assert!(message.contains("read"), "{message}");

    // The same provider re-registering is a refresh, not a duplicate.
    tools
        .register_exclusive(
            spec(50),
            handler("refreshed"),
            "MCP server `docs` tool `read`",
            None,
        )
        .expect("a provider may republish its own tool");
}

fn reference_shaped_spec(input_schema: Value) -> ToolSpec {
    ToolSpec {
        name: "reference_shaped".to_owned(),
        description: "Reference-shaped arguments".to_owned(),
        input_schema,
        output_schema: None,
        config: Value::Null,
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Generic,
        source: ToolSource::BuiltIn,
        selection_priority: 0,
    }
}

async fn invoke_reference_shaped(
    input_schema: Value,
    arguments: Value,
) -> Result<ToolExecutionOutput, ToolError> {
    let registry = ToolRegistry::default();
    registry
        .register(reference_shaped_spec(input_schema), echo_handler())
        .expect("register");
    registry
        .invoke(
            "reference_shaped",
            ToolInvocation {
                call_id: "call-1".to_owned(),
                arguments,
            },
        )
        .await
}

fn todo_shaped_schema() -> Value {
    ObjectSchema::new()
        .define(
            "TodoStatus",
            Property::string().constrained("enum", json!(["pending", "completed"])),
        )
        .define(
            "TodoItem",
            ObjectSchema::new()
                .required("id", Property::string())
                .optional(
                    "status",
                    Property::reference("TodoStatus").with_default("pending"),
                ),
        )
        .required("action", Property::string())
        .optional(
            "todos",
            Property::array(Property::reference("TodoItem"))
                .with_default(Value::Null)
                .nullable(),
        )
        .build()
}

#[test]
fn registration_accepts_reference_constructs_and_rejects_broken_pointers() {
    reference_shaped_spec(todo_shaped_schema())
        .validate()
        .expect("defs, refs and anyOf are accepted");

    let unresolved = reference_shaped_spec(json!({
        "type": "object",
        "properties": {"todos": {"items": {"$ref": "#/$defs/TodoItem"}, "type": "array"}},
    }))
    .validate()
    .expect_err("unresolved pointer");
    assert!(
        unresolved
            .to_string()
            .contains("unresolved $ref `#/$defs/TodoItem` at /properties/todos/items/$ref"),
        "{unresolved}"
    );

    let undeclared = reference_shaped_spec(json!({
        "type": "object",
        "properties": {"action": {"type": "string"}},
        "required": ["action", "todos"],
    }))
    .validate()
    .expect_err("required property absent from properties");
    assert!(
        undeclared
            .to_string()
            .contains("required property `todos` is absent from properties"),
        "{undeclared}"
    );
}

#[tokio::test]
async fn arguments_validate_with_reference_semantics() {
    let schema = todo_shaped_schema();

    let resolved = invoke_reference_shaped(
        schema.clone(),
        json!({"action": "write", "todos": [{"id": "a", "status": "completed"}]}),
    )
    .await
    .expect("referenced subschema accepts a valid item");
    assert_eq!(resolved.typed_result["todos"][0]["status"], "completed");

    invoke_reference_shaped(schema.clone(), json!({"action": "read", "todos": null}))
        .await
        .expect("the null branch of anyOf is accepted");

    let wrong_variant =
        invoke_reference_shaped(schema.clone(), json!({"action": "read", "todos": 7}))
            .await
            .expect_err("a third type is rejected");
    assert!(
        matches!(&wrong_variant, ToolError::InvalidArguments { violations, .. }
            if violations.len() == 1 && violations[0].path == "$.todos"),
        "{wrong_variant}"
    );

    let bad_element = invoke_reference_shaped(
        schema.clone(),
        json!({"action": "write", "todos": [{"id": "a"}, {"id": 7}]}),
    )
    .await
    .expect_err("an element violating the item schema is rejected");
    assert!(
        matches!(&bad_element, ToolError::InvalidArguments { violations, .. }
            if violations.len() == 1 && violations[0].path == "$.todos[1].id"),
        "{bad_element}"
    );

    let enum_violation = invoke_reference_shaped(
        schema.clone(),
        json!({"action": "write", "todos": [{"id": "a", "status": "archived"}]}),
    )
    .await
    .expect_err("a value outside the referenced enum is rejected");
    assert!(
        matches!(&enum_violation, ToolError::InvalidArguments { violations, .. }
            if violations.len() == 1 && violations[0].path == "$.todos[0].status"),
        "{enum_violation}"
    );
}

#[tokio::test]
async fn absent_defaults_reach_the_handler_and_unknown_keys_pass_through() {
    let filled = invoke_reference_shaped(
        todo_shaped_schema(),
        json!({"action": "write", "todos": [{"id": "a"}], "hallucinated": "kept"}),
    )
    .await
    .expect("permissive schemas ignore unknown keys, as Pydantic does");
    assert_eq!(filled.typed_result["todos"][0]["status"], "pending");
    assert_eq!(filled.typed_result["hallucinated"], "kept");

    let strict = ObjectSchema::new()
        .required("task", Property::string())
        .optional("agent", Property::string().with_default("explore"))
        .forbid_extra_properties()
        .build();
    let refused = invoke_reference_shaped(strict.clone(), json!({"task": "go", "extra": 1}))
        .await
        .expect_err("extra=forbid rejects unknown keys");
    assert!(
        matches!(&refused, ToolError::InvalidArguments { violations, .. }
            if violations.len() == 1
                && violations[0].path == "$.extra"
                && violations[0].message.contains("additional property")),
        "{refused}"
    );
    let defaulted = invoke_reference_shaped(strict, json!({"task": "go"}))
        .await
        .expect("default applies");
    assert_eq!(defaulted.typed_result["agent"], "explore");
}

/// The rejection a model reads back names the tool and every argument.
///
/// The reference raises from the tool wrapper and Pydantic collects, so a
/// call breaking three arguments is answered with three. Reporting the
/// first alone would send the model round the loop once per broken
/// argument, and this goes through `invoke` so what is measured is the
/// error a real call produces rather than one a helper composed.
#[tokio::test]
async fn a_rejection_names_the_tool_and_every_argument_it_refused() {
    let schema = ObjectSchema::new()
        .required("action", Property::string())
        .required("count", Property::integer())
        .optional("label", Property::string())
        .build();
    let refused = invoke_reference_shaped(
        schema,
        json!({"action": 1, "count": "seventeen", "label": []}),
    )
    .await
    .expect_err("three arguments carry the wrong type");
    let ToolError::InvalidArguments { tool, violations } = &refused else {
        panic!("expected an argument rejection, got {refused}");
    };
    assert_eq!(tool, "reference_shaped");
    assert_eq!(
        violations
            .iter()
            .map(|violation| violation.path.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["$.action", "$.count", "$.label"])
    );
    let message = refused.to_string();
    assert!(message.contains("reference_shaped"), "{message}");
    for path in ["$.action", "$.count", "$.label"] {
        assert!(message.contains(path), "{message}");
    }
}

#[tokio::test]
async fn a_reference_cycle_terminates_with_a_bounded_depth_error() {
    let schema = json!({
        "$defs": {"Loop": {"$ref": "#/$defs/Loop"}},
        "type": "object",
        "properties": {"node": {"$ref": "#/$defs/Loop"}},
    });
    reference_shaped_spec(schema.clone())
        .validate()
        .expect("the pointer resolves, the cycle is a runtime concern");
    let bounded = invoke_reference_shaped(schema, json!({"node": {}}))
        .await
        .expect_err("cycle is bounded");
    assert!(
        matches!(&bounded, ToolError::InvalidArguments { violations, .. }
        if violations.iter().any(|violation| {
            violation.message.contains("exceeds the 32-level bound")
        })),
        "{bounded}"
    );
}

#[tokio::test]
async fn declared_bounds_reject_what_pydantic_rejects() {
    let schema = ObjectSchema::new()
        .required(
            "questions",
            Property::array(
                ObjectSchema::new()
                    .required("question", Property::string())
                    .required(
                        "options",
                        Property::array(Property::string()).constrained("minItems", 2),
                    ),
            )
            .constrained("minItems", 1),
        )
        .optional(
            "offset",
            Property::integer()
                .constrained("minimum", 1)
                .with_default(Value::Null)
                .nullable(),
        )
        .build();

    let too_few = invoke_reference_shaped(
        schema.clone(),
        json!({"questions": [{"question": "why", "options": ["a"]}]}),
    )
    .await
    .expect_err("minItems is enforced");
    assert!(
        matches!(&too_few, ToolError::InvalidArguments { violations, .. }
            if violations.len() == 1 && violations[0].path == "$.questions[0].options"),
        "{too_few}"
    );

    let below_minimum = invoke_reference_shaped(
        schema.clone(),
        json!({"questions": [{"question": "why", "options": ["a", "b"]}], "offset": 0}),
    )
    .await
    .expect_err("minimum is enforced inside the non-null variant");
    assert!(
        matches!(&below_minimum, ToolError::InvalidArguments { violations, .. }
            if violations.len() == 1
                && violations[0].path == "$.offset"
                && violations[0].message.contains("minimum")),
        "{below_minimum}"
    );

    let empty_list = invoke_reference_shaped(schema, json!({"questions": []}))
        .await
        .expect_err("minItems is enforced on the outer array");
    assert!(
        matches!(&empty_list, ToolError::InvalidArguments { violations, .. }
            if violations.len() == 1 && violations[0].path == "$.questions"),
        "{empty_list}"
    );
}

#[tokio::test]
async fn registry_exposes_typed_contract_and_later_equal_priority_wins() {
    let registry = ToolRegistry::default();
    assert_eq!(
        registry
            .register(spec(10), handler("first"))
            .expect("register"),
        RegistrationOutcome::Inserted
    );
    assert_eq!(
        registry
            .register(spec(10), handler("second"))
            .expect("replace"),
        RegistrationOutcome::Replaced
    );
    let listed = registry.list().expect("list");
    assert_eq!(listed[0].config["maxBytes"], 1024);
    assert_eq!(listed[0].state["calls"], 0);
    let result = registry
        .invoke(
            "read",
            ToolInvocation {
                call_id: "call-1".to_owned(),
                arguments: json!({"path": "README.md"}),
            },
        )
        .await
        .expect("invoke");
    assert_eq!(result.typed_result["content"], "second");
}

fn named_spec(name: &str, source: ToolSource, priority: i32) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        source,
        ..spec(priority)
    }
}

/// Registers the shape a session builds: builtins in their own order, then
/// the MCP tools integration adds, then the connector tools. Every name is
/// chosen so the alphabetical order and the discovery order differ.
fn discovery_ordered_registry() -> (ToolRegistry, Vec<String>) {
    let registry = ToolRegistry::default();
    let registrations = [
        ("write", ToolSource::BuiltIn),
        ("read", ToolSource::BuiltIn),
        ("shell", ToolSource::BuiltIn),
        ("serena_find_symbol", ToolSource::Mcp),
        ("alpha_list", ToolSource::Mcp),
        ("connector_Drive_search", ToolSource::Connector),
    ];
    for (name, source) in registrations {
        registry
            .register(named_spec(name, source, 10), handler("body"))
            .expect("register");
    }
    let expected = registrations
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect::<Vec<_>>();
    (registry, expected)
}

fn published_names(registry: &ToolRegistry) -> Vec<String> {
    registry
        .available(None, &NameFilter::default())
        .expect("available")
        .into_iter()
        .map(|spec| spec.name)
        .collect()
}

/// Reference `available_tool_specs` walks `_all_tools`, an insertion-ordered
/// dict, and both filters in `available_tools` preserve that order. The
/// builtins come first in the order they registered, the MCP tools follow in
/// the order they were integrated, and the connector tools follow those.
#[test]
fn the_published_surface_keeps_the_discovery_order() {
    let (registry, expected) = discovery_ordered_registry();

    assert_eq!(published_names(&registry), expected);
    assert_eq!(
        registry
            .list()
            .expect("list")
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>(),
        expected,
        "reference `registered_tools` walks the same dict"
    );
    let mut alphabetical = expected.clone();
    alphabetical.sort();
    assert_ne!(
        expected, alphabetical,
        "the fixture has to separate the two orders for the assertion to mean anything"
    );
}

/// Two sessions built from the same configuration publish the same order,
/// which is what makes a diff of two model requests readable.
#[test]
fn the_same_registrations_publish_the_same_order_twice() {
    let (first, _) = discovery_ordered_registry();
    let (second, _) = discovery_ordered_registry();

    assert_eq!(published_names(&first), published_names(&second));
}

/// A dict keeps a reassigned key at its original position, so the variant
/// that wins the name is published where the name was first seen rather
/// than at the end of the surface.
#[test]
fn a_replacing_variant_keeps_the_position_of_the_first_registration() {
    let registry = ToolRegistry::default();
    for (name, priority) in [("read", 10), ("write", 10), ("read", 50)] {
        registry
            .register(
                named_spec(name, ToolSource::BuiltIn, priority),
                handler("x"),
            )
            .expect("register");
    }

    let published = registry
        .available(None, &NameFilter::default())
        .expect("available");
    assert_eq!(
        published
            .iter()
            .map(|spec| spec.name.clone())
            .collect::<Vec<_>>(),
        ["read", "write"]
    );
    assert_eq!(
        published[0].selection_priority, 50,
        "the winning variant is the one published"
    );
}

/// Withholding a tool removes it from the surface without resequencing what
/// is left, so a filtered surface is a subsequence of the unfiltered one.
#[test]
fn a_withheld_tool_leaves_the_remaining_order_unchanged() {
    let (registry, expected) = discovery_ordered_registry();
    registry
        .register_conditional(
            named_spec("never", ToolSource::BuiltIn, 10),
            handler("x"),
            Arc::new(|| false),
        )
        .expect("register");

    assert_eq!(
        published_names(&registry),
        expected,
        "an unmet condition removes the tool without resequencing the rest"
    );
    assert_eq!(
        registry
            .available(None, &NameFilter::new(&["shell"]))
            .expect("available")
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>(),
        expected
            .iter()
            .filter(|name| *name != "shell")
            .cloned()
            .collect::<Vec<_>>(),
        "and neither does the denylist"
    );
}

/// Reference `_select_available_variant` ranks the variants published under
/// one name by `(selection_priority, discovery_index)`, so the highest
/// priority owns the name whichever order the variants register in.
#[tokio::test]
async fn the_highest_priority_variant_owns_the_published_name() {
    for order in [[10, 50], [50, 10]] {
        let registry = ToolRegistry::default();
        for (index, priority) in order.into_iter().enumerate() {
            let content = if priority == 50 { "managed" } else { "legacy" };
            let outcome = registry
                .register(spec(priority), handler(content))
                .expect("register");
            assert_eq!(
                outcome,
                if index == 0 {
                    RegistrationOutcome::Inserted
                } else if priority == 50 {
                    RegistrationOutcome::Replaced
                } else {
                    RegistrationOutcome::IgnoredLowerPriority
                }
            );
        }
        let result = registry
            .invoke(
                "read",
                ToolInvocation {
                    call_id: "call-1".to_owned(),
                    arguments: json!({"path": "README.md"}),
                },
            )
            .await
            .expect("invoke");
        assert_eq!(result.typed_result["content"], "managed", "{order:?}");
    }
}

/// The second half of the same rank: reference `_select_available_variant`
/// keeps a candidate only when `(priority, discovery_index)` is strictly
/// greater than the one it holds, and the discovery index grows, so two
/// variants of equal priority leave the name to the one discovered last.
#[tokio::test]
async fn variants_of_equal_priority_leave_the_name_to_the_last_discovered() {
    let registry = ToolRegistry::default();
    assert_eq!(
        registry
            .register(spec(10), handler("first"))
            .expect("first"),
        RegistrationOutcome::Inserted
    );
    assert_eq!(
        registry
            .register(spec(10), handler("second"))
            .expect("second"),
        RegistrationOutcome::Replaced,
        "the tie is broken by discovery order, not by the first claim"
    );
    let result = registry
        .invoke(
            "read",
            ToolInvocation {
                call_id: "call-1".to_owned(),
                arguments: json!({"path": "README.md"}),
            },
        )
        .await
        .expect("invoke");
    assert_eq!(result.typed_result["content"], "second");
}

/// Reference `is_available` is asked again on every `available_tools`
/// access, so a prerequisite that comes and goes moves the tool in and out
/// of the published surface without a re-registration.
#[tokio::test]
async fn a_conditional_tool_follows_its_prerequisite_at_publication_time() {
    let present = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe = Arc::clone(&present);
    let registry = ToolRegistry::default();
    registry
        .register_conditional(
            spec(0),
            handler("conditional"),
            Arc::new(move || probe.load(Ordering::Acquire)),
        )
        .expect("register");
    let published = || {
        registry
            .available(None, &NameFilter::default())
            .expect("available")
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>()
    };
    // Registered, so it is listed, and withheld, so nothing publishes it.
    assert_eq!(registry.list().expect("list").len(), 1);
    assert!(published().is_empty());
    assert_eq!(registry.withheld().expect("withheld"), ["read".to_owned()]);
    assert!(matches!(
        registry
            .invoke(
                "read",
                ToolInvocation {
                    call_id: "call-1".to_owned(),
                    arguments: json!({"path": "README.md"}),
                },
            )
            .await,
        Err(ToolError::Unavailable(name)) if name == "read"
    ));

    present.store(true, Ordering::Release);
    assert_eq!(published(), ["read".to_owned()]);
    assert!(registry.withheld().expect("withheld").is_empty());
    assert!(
        registry
            .invoke(
                "read",
                ToolInvocation {
                    call_id: "call-2".to_owned(),
                    arguments: json!({"path": "README.md"}),
                },
            )
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn schema_invalid_arguments_and_results_fail_closed() {
    let registry = ToolRegistry::default();
    registry.register(spec(0), handler("ok")).expect("register");
    let missing = registry
        .invoke(
            "read",
            ToolInvocation {
                call_id: "call-1".to_owned(),
                arguments: json!({}),
            },
        )
        .await;
    assert!(matches!(missing, Err(ToolError::InvalidArguments { .. })));

    let invalid_result: Arc<dyn ToolHandler> = Arc::new(
        |_invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
            Box::pin(async {
                Ok(ToolExecutionOutput::new("invalid".to_owned()).typed(json!({"content": 7})))
            })
        },
    );
    registry.register(spec(1), invalid_result).expect("replace");
    let result = registry
        .invoke(
            "read",
            ToolInvocation {
                call_id: "call-2".to_owned(),
                arguments: json!({"path": "README.md"}),
            },
        )
        .await;
    assert!(matches!(result, Err(ToolError::SchemaViolation { .. })));
}

#[tokio::test]
async fn unavailable_and_oversized_outputs_are_bounded() {
    let registry = ToolRegistry::new(8);
    registry
        .register(spec(0), handler("larger than eight bytes"))
        .expect("register");
    let result = registry
        .invoke(
            "read",
            ToolInvocation {
                call_id: "call-1".to_owned(),
                arguments: json!({"path": "README.md"}),
            },
        )
        .await;
    assert!(matches!(result, Err(ToolError::OutputTooLarge { .. })));
    assert!(matches!(
        registry
            .invoke(
                "missing",
                ToolInvocation {
                    call_id: "call-2".to_owned(),
                    arguments: json!({}),
                },
            )
            .await,
        Err(ToolError::Unavailable(_))
    ));
}

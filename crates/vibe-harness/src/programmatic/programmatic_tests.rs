use serde_json::json;

use super::*;

#[test]
fn the_inventory_keeps_the_reference_order() {
    let names: Vec<String> = inventory().iter().map(Callable::qualified).collect();
    assert_eq!(
        names,
        [
            "file_system.bash",
            "file_system.read_file",
            "file_system.write_file",
            "file_system.edit",
            "self.sleep",
            "process.start",
            "process.output",
            "process.write",
            "process.list",
            "process.stop",
            "subagent.list",
            "subagent.spawn",
            "subagent.wait",
            "subagent.sendMessage",
            "subagent.interrupt",
            "subagent.close",
            "vibe.todo",
        ]
    );
}

#[test]
fn arguments_that_do_not_fit_are_reported_as_ajv_does() {
    let callables = inventory();
    let read = callables
        .iter()
        .find(|callable| callable.qualified() == "file_system.read_file")
        .unwrap();
    let error = argument_error(read, &json!({})).expect("path is required");
    assert_eq!(error["type"], "jsonSchemaArgumentValidationError");
    assert_eq!(error["functionName"], "file_system.read_file");
    assert_eq!(
        error["argumentsValidationErrors"][0]["message"],
        "must have required property 'path'"
    );
    assert!(argument_error(read, &json!({"path": "a"})).is_none());
}

#[test]
fn a_result_settles_to_its_structured_content_or_its_error() {
    let success = ToolResult::Success {
        content: Vec::new(),
        structured_content: Some(json!({"b": 1, "a": 2})),
        meta: None,
    };
    assert_eq!(
        serde_json::to_string(&settled_value(&success).unwrap()).unwrap(),
        r#"{"a":2,"b":1}"#
    );
    let failure = ToolResult::failed("tool_failed", "missing");
    assert_eq!(
        settled_value(&failure).unwrap_err(),
        json!({"name": "tool_failed", "message": "missing", "details": null})
    );
}

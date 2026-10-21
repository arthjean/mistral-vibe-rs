use serde_json::json;

use super::*;

#[test]
fn remote_names_resolve_the_way_the_reference_names_them() {
    let names = programmatic_names(["create-issue", "search", "9lives", "a.b", "a-b"].into_iter());
    assert_eq!(names["search"], "search");
    assert_eq!(names["create-issue"], "create_issue");
    assert_eq!(names["9lives"], "_9lives");
    // `a-b` and `a.b` share `a_b`, so each adds a digest of its own name.
    assert!(names["a-b"].starts_with("a_b_"));
    assert!(names["a.b"].starts_with("a_b_"));
    assert_ne!(names["a-b"], names["a.b"]);
    assert_eq!(names["a-b"].len(), "a_b_".len() + 8);
}

#[test]
fn group_names_follow_the_provider_kind() {
    assert_eq!(
        group_candidate(RemoteToolKind::Mcp, "data-api"),
        "mcp_data_api"
    );
    assert_eq!(
        group_candidate(RemoteToolKind::Connector, "linear"),
        "connector_linear"
    );
}

#[test]
fn the_registry_description_splits_into_the_tool_and_its_hint() {
    assert_eq!(
        registered_description("fixture", "[fixture] Echo the text\nHint: Use it to test"),
        (
            "Echo the text".to_owned(),
            Some("Use it to test".to_owned())
        )
    );
    assert_eq!(
        registered_description("fixture", "[fixture] Echo the text"),
        ("Echo the text".to_owned(), None)
    );
}

#[test]
fn a_result_keeps_its_blocks_and_an_error_flag_fails_the_call() {
    let codes = Codes::of(RemoteToolKind::Mcp);
    let success = normalized(
        &json!({"content": [{"type": "text", "text": "hi"}], "structuredContent": {"a": 1}, "_meta": {"k": true}}),
        codes,
    );
    assert_eq!(
        success,
        ToolResult::Success {
            content: vec![json!({"type": "text", "text": "hi"})],
            structured_content: Some(json!({"a": 1})),
            meta: json!({"k": true}).as_object().cloned(),
        }
    );
    let failure = |result: ToolResult| match result {
        ToolResult::Failure { code, message, .. } => Some((code, message)),
        ToolResult::Success { .. } => None,
    };
    let error = normalized(
        &json!({"content": [{"type": "text", "text": "boom"}], "isError": true}),
        codes,
    );
    assert_eq!(
        failure(error),
        Some((
            "mcp_tool_error".to_owned(),
            "The MCP tool returned an error".to_owned()
        ))
    );
    let invalid = normalized(&json!({"content": [{"type": "video"}]}), codes);
    assert_eq!(
        failure(invalid),
        Some(("mcp_invalid_result".to_owned(), CALL_FAILED.to_owned()))
    );
}

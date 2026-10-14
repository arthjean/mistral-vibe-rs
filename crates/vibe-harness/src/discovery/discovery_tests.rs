use serde_json::json;

use super::*;

fn functions() -> Vec<Function> {
    let input = json!({
        "type": "object",
        "properties": {
            "processId": {"type": "string", "minLength": 1},
            "env": {"type": "object", "additionalProperties": {"type": "string"}},
        },
        "required": ["processId"],
        "additionalProperties": false,
    });
    let output = json!({
        "type": "object",
        "properties": {"status": {"type": "string", "enum": ["running", "completed"]}},
        "required": ["status"],
    });
    vec![
        Function::new(
            "process",
            "Background commands.",
            "stop",
            "Stop a background process.",
            &input,
            Some(&output),
        ),
        Function::new(
            "file_system",
            "Files.",
            "read_file",
            "Read a text file.",
            &json!({"type": "object", "properties": {}, "additionalProperties": false}),
            None,
        ),
    ]
}

fn request(arguments: serde_json::Value) -> SearchRequest {
    SearchRequest::parse(&arguments).expect("the arguments are valid")
}

#[test]
fn a_declaration_renders_the_schemas_as_typescript() {
    let declaration = &functions()[0].declaration;
    assert_eq!(
        declaration,
        "declare namespace tools.process {\n  type StopArg = {\n    /**\n     * @minLength 1\n     */\n    processId: string;\n    env?: Record<string, string>;\n  };\n  type StopReturned = {\n    status: \"completed\" | \"running\";\n  };\n  /**\n   * Stop a background process.\n   */\n  function stop(arg: StopArg): Promise<StopReturned>;\n}"
    );
    assert!(
        functions()[1]
            .declaration
            .contains("type ReadFileArg = Record<string, never>;")
    );
    assert!(
        functions()[1]
            .declaration
            .contains("type ReadFileReturned = unknown;")
    );
}

#[test]
fn best_match_ranks_by_bm25_and_names_the_connectors() {
    let answer = search(&functions(), request(json!({"query": "stop process"})));
    assert_eq!(answer.tool_count, 1);
    assert!(
        answer
            .content
            .contains("# Best Matching Tool Candidates\n\n1. process.stop - ")
    );
    assert!(
        answer
            .content
            .contains("<name>process</name>\n    <status>connected</status>")
    );
    let empty = search(&functions(), request(json!({"query": "zzzz"})));
    assert!(empty.content.ends_with(
        "No matching tool functions found for query \"zzzz\". Try a broader search query."
    ));
}

#[test]
fn details_and_capabilities_follow_the_reference_layout() {
    let answer = search(
        &functions(),
        request(json!({"functions": ["tools.process.stop", "stop", "nope"]})),
    );
    assert_eq!(answer.mode, Mode::Details);
    assert_eq!(answer.tool_count, 1);
    assert!(answer.content.starts_with("---\nmode: details\nrelevantFunctionCount: 1\nrelatedFunctionCount: 1\nrelatedConnectorCount: 1\nrelevantConnectorNoticeCount: 0\nrequestedFunctionCount: 3\nloadedFunctionCount: 1\nloadedFunctions:\n  - name: process.stop\n    connector: process\n---"));
    assert!(
        answer
            .content
            .ends_with("# Missing Exact Names\n\n- No exact tool function found for: nope")
    );
    let listed = search(
        &functions(),
        request(json!({"mode": "all_connector_capabilities", "connectors": ["file_system"]})),
    );
    assert!(
        listed
            .content
            .ends_with("# All Connector Capabilities\n\n1. file_system.read_file")
    );
}

#[test]
fn malformed_arguments_are_refused_with_the_reference_message() {
    assert_eq!(
        SearchRequest::parse(&json!({"functions": [""]})).unwrap_err(),
        "invalid search_tool_functions arguments: functions values must not be empty"
    );
    assert!(SearchRequest::parse(&json!({"mode": "everything"})).is_err());
}

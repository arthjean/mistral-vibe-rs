use serde_json::json;

use super::*;

#[test]
fn every_tool_maps_to_the_kind_it_presents_as() {
    for (tool, expected) in [
        ("bash", ToolEffectKind::Shell),
        ("edit", ToolEffectKind::FileEdit),
        ("grep", ToolEffectKind::FileSearch),
        ("read_file", ToolEffectKind::FileRead),
        ("todo", ToolEffectKind::Todo),
        ("write_file", ToolEffectKind::FileWrite),
        ("ask_user_question", ToolEffectKind::UserQuestion),
        ("web_search", ToolEffectKind::WebSearch),
        ("web_fetch", ToolEffectKind::WebFetch),
        ("skill", ToolEffectKind::Skill),
        ("task", ToolEffectKind::Subagent),
        ("mcp_fixture_echo", ToolEffectKind::Tool),
    ] {
        assert_eq!(ToolEffectKind::from_tool_name(tool), expected, "{tool}");
    }
}

#[test]
fn a_detail_carries_neither_the_call_id_nor_the_raw_arguments() {
    let detail = EffectDetail::for_encoded_call("bash", r#"{"command":"ls","timeout":5}"#);
    let wire = serde_json::to_value(&detail).expect("serializes");
    let object = wire.as_object().expect("an object");
    assert!(!object.contains_key("toolCallId"), "{wire}");
    assert!(!object.contains_key("arguments"), "{wire}");
    assert_eq!(
        object.keys().collect::<Vec<_>>(),
        ["display", "input", "kind", "toolName"]
    );
    // The shell input model declares `command` and nothing else, so the
    // surplus argument is dropped rather than forwarded.
    assert_eq!(wire["input"], json!({"command": "ls"}));
}

#[test]
fn a_call_display_carries_all_eight_reference_fields() {
    let detail = EffectDetail::for_encoded_call("read_file", r#"{"file_path":"/tmp/a.txt"}"#);
    let display = serde_json::to_value(&detail.display).expect("serializes");
    let mut keys = display
        .as_object()
        .expect("an object")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    assert_eq!(
        keys,
        [
            "content",
            "message",
            "settledMessage",
            "settledVerb",
            "statusText",
            "suffix",
            "summary",
            "verb",
        ]
    );
    assert_eq!(display["verb"], "Reading");
    assert_eq!(display["settledVerb"], "Read");
    assert_eq!(display["statusText"], "Reading file");
    assert_eq!(display["summary"], "Reading /tmp/a.txt");
}

#[test]
fn a_generic_effect_publishes_an_object_input_even_with_no_arguments() {
    let detail = EffectDetail::for_encoded_call("mcp_fixture_echo", "not json");
    assert_eq!(detail.kind, ToolEffectKind::Tool);
    assert_eq!(detail.input, json!({}));
    assert_eq!(detail.display.summary, "mcp_fixture_echo()");
}

#[test]
fn each_kind_projects_its_input_onto_the_fields_its_model_declares() {
    let cases: [(&str, &str, ToolEffectKind, serde_json::Value); 11] = [
        (
            "bash",
            r#"{"command":"ls -a"}"#,
            ToolEffectKind::Shell,
            json!({"command": "ls -a"}),
        ),
        (
            "edit",
            r#"{"file_path":"/a","old_string":"x","new_string":"y","replace_all":true}"#,
            ToolEffectKind::FileEdit,
            json!({"filePath": "/a", "oldString": "x", "newString": "y", "replaceAll": true}),
        ),
        (
            "grep",
            r#"{"pattern":"fn","path":"src","max_matches":10}"#,
            ToolEffectKind::FileSearch,
            json!({"pattern": "fn", "path": "src", "maxMatches": 10}),
        ),
        (
            "read_file",
            r#"{"file_path":"/a","offset":3}"#,
            ToolEffectKind::FileRead,
            json!({"filePath": "/a", "offset": 3, "limit": 2000}),
        ),
        (
            "todo",
            r#"{"action":"read"}"#,
            ToolEffectKind::Todo,
            json!({"action": "read", "todos": null}),
        ),
        (
            "write_file",
            r#"{"file_path":"/a","content":"hi"}"#,
            ToolEffectKind::FileWrite,
            json!({"filePath": "/a", "content": "hi"}),
        ),
        (
            "ask_user_question",
            r#"{"questions":[]}"#,
            ToolEffectKind::UserQuestion,
            json!({"questions": [], "footerNote": null}),
        ),
        (
            "web_search",
            r#"{"query":"rust"}"#,
            ToolEffectKind::WebSearch,
            json!({"query": "rust"}),
        ),
        (
            "web_fetch",
            r#"{"url":"https://example.com/a"}"#,
            ToolEffectKind::WebFetch,
            json!({"url": "https://example.com/a", "timeout": null}),
        ),
        (
            "skill",
            r#"{"name":"probe"}"#,
            ToolEffectKind::Skill,
            json!({"name": "probe"}),
        ),
        (
            "task",
            r#"{"task":"audit","agent":"explore"}"#,
            ToolEffectKind::Subagent,
            json!({"task": "audit", "agent": "explore"}),
        ),
    ];
    for (tool, arguments, kind, input) in cases {
        let detail = EffectDetail::for_encoded_call(tool, arguments);
        assert_eq!(detail.kind, kind, "{tool}");
        assert_eq!(detail.input, input, "{tool}");
    }
}

#[test]
fn only_the_subagent_kind_publishes_a_child_session() {
    let mut subagent = EffectDetail::for_encoded_call("task", r#"{"task":"a","agent":"b"}"#);
    subagent.child_session_id = Some("session-child".to_owned());
    let wire = serde_json::to_value(&subagent).expect("serializes");
    assert_eq!(wire["childSessionId"], "session-child");

    let shell = EffectDetail::for_encoded_call("bash", r#"{"command":"ls"}"#);
    let wire = serde_json::to_value(&shell).expect("serializes");
    assert!(
        !wire
            .as_object()
            .expect("an object")
            .contains_key("childSessionId"),
        "{wire}"
    );
}

#[test]
fn a_settled_display_names_the_subject_the_call_named() {
    let detail = EffectDetail::for_encoded_call("bash", r#"{"command":"cargo test"}"#);
    let failed = EffectResultDisplay::failed(&detail.display);
    assert_eq!(failed.verb, "Ran");
    assert_eq!(failed.message, "cargo test");
    assert!(!failed.success);

    let completed = EffectResultDisplay::completed(
        detail.kind,
        &detail.display,
        &json!({"command": "cargo test", "stdout": "ok"}),
        &Value::Null,
    );
    assert!(completed.success);
    assert_eq!(completed.verb, "Ran");
    assert_eq!(completed.message, "cargo test");
}

#[test]
fn a_tool_emitted_result_display_stays_authoritative() {
    let detail = EffectDetail::for_encoded_call("bash", r#"{"command":"ls"}"#);
    let emitted = json!({"success": false, "verb": "Refused", "message": "sandbox"});
    let display =
        EffectResultDisplay::completed(detail.kind, &detail.display, &json!({}), &emitted);
    assert!(!display.success);
    assert_eq!(display.verb, "Refused");
    assert_eq!(display.message, "sandbox");
}

#[test]
fn a_cancellation_with_no_result_carries_no_display() {
    assert_eq!(
        EffectResultDisplay::cancelled("bash", &Value::Null, &Value::Null),
        None
    );
    assert_eq!(
        EffectResultDisplay::cancelled("bash", &json!({"stdout": ""}), &Value::Null)
            .expect("a display")
            .message,
        "bash: skipped"
    );
}

#[test]
fn a_notice_detail_round_trips_through_its_discriminator() {
    for (detail, kind) in [
        (
            NoticeDetail::HookRunStarted(HookNotice::default()),
            "hook_run_started",
        ),
        (
            NoticeDetail::HookCompleted(HookNotice {
                scope: HookScope::PreTool,
                tool_name: Some("bash".to_owned()),
                tool_call_id: Some("call-1".to_owned()),
                hook_name: Some("guard".to_owned()),
                status: Some(HookSeverity::Warning),
                content: Some("slow".to_owned()),
            }),
            "hook_completed",
        ),
        (
            NoticeDetail::AgentChanged {
                agent_name: "explore".to_owned(),
            },
            "agent_changed",
        ),
        (
            NoticeDetail::ContextCleared {
                plan_file_path: Some("/plan.md".to_owned()),
            },
            "context_cleared",
        ),
        (
            NoticeDetail::SessionTitleUpdated {
                title: "Audit".to_owned(),
            },
            "session_title_updated",
        ),
        (
            NoticeDetail::PlanReviewStarted {
                file_path: "/plan.md".to_owned(),
            },
            "plan_review_started",
        ),
        (NoticeDetail::PlanReviewEnded, "plan_review_ended"),
        (
            NoticeDetail::WaitingForInput {
                task_id: "task-1".to_owned(),
                label: None,
                predefined_answers: None,
            },
            "waiting_for_input",
        ),
        (
            NoticeDetail::ScheduledLoopFired {
                loop_id: "loop-1".to_owned(),
            },
            "scheduled_loop_fired",
        ),
    ] {
        let wire = serde_json::to_value(&detail).expect("serializes");
        assert_eq!(wire["kind"], kind);
        let parsed: NoticeDetail = serde_json::from_value(wire).expect("parses");
        assert_eq!(parsed, detail);
    }

    // A hook notice publishes its scope even when the caller left the default,
    // which is what makes the four hook kinds share one body.
    let wire =
        serde_json::to_value(NoticeDetail::HookStarted(HookNotice::default())).expect("serializes");
    assert_eq!(wire["scope"], "post_agent");
}

#[test]
fn a_callback_output_names_the_form_it_answers() {
    let approval = CallbackOutput::Approval {
        decision: ApprovalDecision {
            decision: ApprovalDecisionType::CancelTurn,
        },
        feedback: None,
    };
    let wire = serde_json::to_value(&approval).expect("serializes");
    assert_eq!(wire["type"], "approval");
    assert_eq!(wire["decision"]["type"], "cancel_turn");
    assert!(approval.requests_turn_cancel());

    let answer = CallbackOutput::UserInput {
        result: UserQuestionResult {
            answers: vec![UserAnswer {
                question: "Ship?".to_owned(),
                answer: "Yes".to_owned(),
                is_other: false,
            }],
            cancelled: false,
        },
    };
    let wire = serde_json::to_value(&answer).expect("serializes");
    assert_eq!(wire["type"], "user_input");
    assert_eq!(wire["result"]["answers"][0]["isOther"], false);
    assert!(!answer.requests_turn_cancel());
}

#[test]
fn a_callback_detail_names_what_it_asks_for() {
    let approval = CallbackDetail::Approval {
        effect: Box::new(EffectDetail::for_encoded_call(
            "bash",
            r#"{"command":"rm -rf /"}"#,
        )),
        required_permissions: vec![PermissionRequirement::command("rm -rf /")],
        choices: ApprovalDecisionType::ALL.to_vec(),
        related_entry_id: Some("entry-1".to_owned()),
    };
    let wire = serde_json::to_value(&approval).expect("serializes");
    assert_eq!(wire["kind"], "approval");
    assert_eq!(wire["effect"]["kind"], "shell");
    assert_eq!(wire["relatedEntryId"], "entry-1");
    assert_eq!(approval.related_entry_id(), Some("entry-1"));
    assert_eq!(wire["choices"][4], "cancel_turn");

    let question = CallbackDetail::UserInput {
        request: UserQuestionRequest {
            questions: vec![UserQuestion {
                question: "Ship?".to_owned(),
                header: "Release".to_owned(),
                options: vec![QuestionChoice {
                    label: "Yes".to_owned(),
                    description: String::new(),
                }],
                multi_select: false,
                hide_other: false,
            }],
            footer_note: None,
        },
        related_entry_id: None,
    };
    let wire = serde_json::to_value(&question).expect("serializes");
    assert_eq!(wire["kind"], "user_input");
    assert_eq!(wire["request"]["questions"][0]["multiSelect"], false);
}

#[test]
fn a_proxied_call_display_names_the_published_tool_and_its_server() {
    let mcp = EffectDetail::for_proxied_call(
        "mcp_docs_search",
        r#"{"query":"parity"}"#,
        &RemoteToolOrigin::mcp("search"),
    );
    // The reference's `MCPTool` inherits the base `format_call_display`, which
    // names the tool and renders none of its arguments.
    assert_eq!(mcp.display.summary, "mcp_docs_search");
    assert_eq!(mcp.display.status_text, "Calling MCP tool search");
    assert_eq!(mcp.kind, ToolEffectKind::Tool);

    let connector = EffectDetail::for_proxied_call(
        "connector_github_issue",
        r#"{"number":7}"#,
        &RemoteToolOrigin::connector("issue"),
    );
    assert_eq!(connector.display.summary, "connector_github_issue");
    assert_eq!(
        connector.display.status_text,
        "Calling connector tool issue"
    );

    // An unnamed remote leaves the label pointing at the published name rather
    // than trailing off after the verb.
    let unnamed = EffectDetail::for_proxied_call("mcp_bare", "{}", &RemoteToolOrigin::mcp(""));
    assert_eq!(unnamed.display.status_text, "Calling MCP tool mcp_bare");

    // Arguments that never decode reach the same display: the proxy renders
    // none of them, so there is nothing for a broken payload to break.
    for arguments in ["", "{not json", "[]"] {
        let broken = EffectDetail::for_proxied_call(
            "mcp_docs_search",
            arguments,
            &RemoteToolOrigin::mcp("search"),
        );
        assert_eq!(broken.display.summary, "mcp_docs_search", "{arguments}");
        assert_eq!(
            broken.display.status_text, "Calling MCP tool search",
            "{arguments}"
        );
    }
}

#[test]
fn every_call_display_publishes_a_settled_message() {
    // A presentation that names its own subject keeps it across both headers.
    let named = EffectDetail::for_encoded_call("bash", r#"{"command":"echo alpha"}"#);
    assert_eq!(named.display.summary, "bash: echo alpha");
    assert_eq!(named.display.settled_message.as_deref(), Some("echo alpha"));

    // A presentation that names none falls back to the summary, not to the
    // message, which is the rule the reference adapter applies.
    let proxied =
        EffectDetail::for_proxied_call("mcp_docs_search", "{}", &RemoteToolOrigin::mcp("search"));
    assert_eq!(
        proxied.display.settled_message.as_deref(),
        Some("mcp_docs_search")
    );
    assert_eq!(proxied.display.message.as_deref(), Some("mcp_docs_search"));
    assert_eq!(proxied.display.verb, "Running");
    assert_eq!(proxied.display.settled_verb, "Ran");

    // A call whose arguments never arrived still carries both, and neither is
    // the empty string a bare verb would render.
    let argumentless = EffectDetail::for_encoded_call("read_file", "");
    assert_eq!(
        argumentless.display.message.as_deref(),
        Some(argumentless.display.summary.as_str())
    );
    assert_eq!(
        argumentless.display.settled_message.as_deref(),
        Some(argumentless.display.summary.as_str())
    );
    assert!(!argumentless.display.summary.is_empty());
}

#[test]
fn a_stored_display_with_no_settled_message_still_hydrates() {
    // A session recorded before the fill was published carries neither optional
    // header, and hydrating it renders through the collapsed summary.
    let stored = json!({
        "summary": "bash: echo alpha",
        "verb": "Running",
        "settledVerb": "Ran",
        "statusText": "Running command",
    });
    let display: EffectCallDisplay = serde_json::from_value(stored).expect("hydrates");
    assert_eq!(display.message, None);
    assert_eq!(display.settled_message, None);
    assert_eq!(display.subject(), "bash: echo alpha");
}

#[test]
fn the_generic_fallback_spells_its_arguments_the_way_the_interpreter_does() {
    let detail = EffectDetail::for_encoded_call(
        "custom_tool",
        r#"{"a_flag":true,"b_missing":null,"c_nested":{"key":"value"},"d_dropped":1}"#,
    );
    // Three arguments at most, spelled under Python's literal syntax, in the
    // order the call carried them.
    assert_eq!(
        detail.display.summary,
        "custom_tool(a_flag=True, b_missing=None, c_nested={'key': 'value'})"
    );

    let more = EffectDetail::for_encoded_call(
        "custom_tool",
        r#"{"a_off":false,"b_items":["a",2],"c_text":"it's here"}"#,
    );
    assert_eq!(
        more.display.summary,
        r#"custom_tool(a_off=False, b_items=['a', 2], c_text="it's here")"#
    );

    // The reference interpolates the `dict` its model was called with, which
    // keeps insertion order, and a `serde_json::Map` iterates lexicographically
    // instead. Reading the keys back off the call text is what keeps the two
    // agreeing on which three arguments are shown and in which order.
    let unsorted =
        EffectDetail::for_encoded_call("custom_tool", r#"{"zulu":1,"alpha":2,"mike":3,"bravo":4}"#);
    assert_eq!(
        unsorted.display.summary,
        "custom_tool(zulu=1, alpha=2, mike=3)"
    );

    // Arguments that never decoded name no order, and the summary is still the
    // published name with empty parentheses behind it.
    let undecodable = EffectDetail::for_encoded_call("custom_tool", "not json");
    assert_eq!(undecodable.display.summary, "custom_tool()");
}

#[test]
fn a_remote_settlement_reads_the_three_branches_in_reference_order() {
    let remote = RemoteToolOrigin::mcp("search");
    let call = EffectDetail::for_proxied_call("mcp_docs_search", "{}", &remote).display;
    // The shape the reference's proxy builds and the presentation corpus
    // records: `server` is what names it as the proxy's own rather than as the
    // content the remote tool answered with.
    let answered = json!({"ok": true, "server": "docs", "tool": "search"});

    // A result the server answered names the remote tool under the settled verb.
    let completed =
        EffectResultDisplay::for_remote(&remote, &call, &RemoteSettlement::answered(&answered));
    assert!(completed.success);
    assert_eq!(completed.verb, "Ran");
    assert_eq!(completed.message, "search");

    // The connector proxy prefixes the same name.
    let connector = RemoteToolOrigin::connector("issue");
    let connector_call =
        EffectDetail::for_proxied_call("connector_github_issue", "{}", &connector).display;
    let settled = EffectResultDisplay::for_remote(
        &connector,
        &connector_call,
        &RemoteSettlement::answered(&json!({"ok": true, "server": "acme", "tool": "issue"})),
    );
    assert_eq!(settled.message, "connector issue");

    // A proxy result that reported failure settles as one.
    let refused = EffectResultDisplay::for_remote(
        &remote,
        &call,
        &RemoteSettlement::answered(&json!({"ok": false, "server": "docs", "tool": "search"})),
    );
    assert!(!refused.success);
    assert_eq!(refused.message, "search");

    // What this port's peers actually publish is the server's own
    // `structuredContent`, which shares the proxy result's field names without
    // carrying its meaning. Nothing in it answers for the call, so the
    // settlement is what the origin knows.
    for payload in [
        json!({"ok": false, "tool": "the server's own field"}),
        json!({"content": [{"type": "text", "text": "done"}]}),
        json!({}),
    ] {
        let structured =
            EffectResultDisplay::for_remote(&remote, &call, &RemoteSettlement::answered(&payload));
        assert!(structured.success, "{payload}");
        assert_eq!(structured.message, "search", "{payload}");
    }

    // An error outranks the result branch.
    let errored = EffectResultDisplay::for_remote(
        &remote,
        &call,
        &RemoteSettlement::failed("server unavailable", &answered),
    );
    assert!(!errored.success);
    assert_eq!(errored.message, "server unavailable");

    // A skip outranks it too, and names its reason or the default label.
    let skipped = EffectResultDisplay::for_remote(
        &remote,
        &call,
        &RemoteSettlement::skipped("the user declined", &answered),
    );
    assert!(!skipped.success);
    assert_eq!(skipped.verb, "");
    assert_eq!(skipped.message, "the user declined");
    let unexplained =
        EffectResultDisplay::for_remote(&remote, &call, &RemoteSettlement::skipped("", &answered));
    assert_eq!(unexplained.message, "Skipped");

    // An output shaped like nothing the proxy declares settles as a failure
    // rather than taking the process down.
    for output in [json!("a bare string"), json!([1, 2]), Value::Null] {
        let unreadable =
            EffectResultDisplay::for_remote(&remote, &call, &RemoteSettlement::answered(&output));
        assert!(!unreadable.success, "{output}");
        assert_eq!(unreadable.message, "No result", "{output}");
    }

    // A proxy result carrying no tool name falls back to the remote name the
    // origin carries, which is what the proxy knows it called.
    let unnamed = EffectResultDisplay::for_remote(
        &remote,
        &call,
        &RemoteSettlement::answered(&json!({"ok": true, "server": "docs"})),
    );
    assert_eq!(unnamed.message, "search");
}

use std::sync::Arc;

use serde_json::{Value, json};
use vibe_app_server::client::{PublicEntryGenerationStatus, PublicEntryMetadata};

use super::bodies::process_lines;
use super::*;

/// An entry in the shape the app server publishes: a typed detail carrying
/// the call display, and a settled state carrying the result display the
/// projection computed for it.
///
/// `state` is written in its wire form so a fixture reads like the payload
/// a client receives, and is deserialized back into the published union
/// here, which is what the projection under test consumes.
fn effect(tool: &str, arguments: Value, mut state: Value) -> TranscriptEntry {
    let detail = EffectDetail::for_call(tool, &arguments);
    if let Some(object) = state.as_object_mut() {
        let output = object.get("output").cloned().unwrap_or(Value::Null);
        let display = match object.get("status").and_then(Value::as_str) {
            Some("completed") => Some(EffectResultDisplay::completed(
                detail.kind,
                &detail.display,
                &output,
                &Value::Null,
            )),
            Some("failed") => Some(EffectResultDisplay::failed(&detail.display)),
            Some("skipped") => Some(EffectResultDisplay::skipped(&detail.tool_name)),
            Some("cancelled") => {
                EffectResultDisplay::cancelled(&detail.tool_name, &output, &Value::Null)
            }
            _ => None,
        };
        if let Some(display) = display {
            object.insert("display".to_owned(), json!(display));
        }
    }
    published(PublicHistoryEntry::Effect {
        metadata: metadata("effect"),
        title: tool.to_owned(),
        detail: Box::new(detail),
        state: serde_json::from_value(state).expect("the fixture is a published effect state"),
        tool_call_id: String::new(),
    })
}

fn metadata(id: &str) -> PublicEntryMetadata {
    PublicEntryMetadata {
        input_entry_id: None,
        id: id.to_owned(),
        session_id: "session".to_owned(),
        turn_id: None,
        created_at: 1,
        updated_at: 1,
        generation_status: PublicEntryGenerationStatus::Completed,
        related_entry_id: None,
    }
}

/// Projects a published entry exactly the way the session does, so a
/// fixture and a live entry reach the region under test the same way.
fn published(entry: PublicHistoryEntry) -> TranscriptEntry {
    crate::tui::hydration::history_entry(entry)
}

fn effect_of(entry: &TranscriptEntry) -> EffectRegion {
    match region(entry) {
        Region::Effect(effect) => *effect,
        other => panic!("expected an effect region, got {other:?}"),
    }
}

/// The transcript renders the payload the `todo` tool actually produces,
/// taken from the tool itself rather than from a hand-written fixture, so
/// a change to either side breaks here.
#[tokio::test]
async fn the_todo_tool_result_renders_through_the_todo_effect() {
    let directory = tempfile::tempdir().expect("tempdir");
    let policy = vibe_core::policy::PermissionStore::default();
    let registry = vibe_core::tools::ToolRegistry::default();
    vibe_core::tools::builtins::BuiltinTools::new(directory.path(), None)
        .register(
            "session-1",
            vibe_core::skills::SkillDiscovery::default(),
            &registry,
            &vibe_core::policy::ToolGuard::new(policy, Arc::new(DenyEverything)),
        )
        .expect("universal tools register");
    let written = registry
        .invoke(
            "todo",
            vibe_core::tools::ToolInvocation {
                call_id: "todo-1".to_owned(),
                arguments: json!({
                    "action": "write",
                    "todos": [
                        {"id": "a", "content": "draft", "status": "completed"},
                        {"id": "b", "content": "ship", "status": "in_progress"}
                    ],
                }),
            },
        )
        .await
        .expect("write");

    let entry = effect(
        "todo",
        json!({"action": "write"}),
        json!({
            "status": "completed",
            "output": written.typed_result,
            "outputText": written.model_text,
        }),
    );
    assert_eq!(EffectKind::from_tool_name("todo"), EffectKind::Todo);
    let rendered = effect_of(&entry);
    // Reference `TodoResultWidget` order: in_progress before completed.
    assert_eq!(
        rendered
            .body
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>(),
        ["☐ ship", "☑ draft"]
    );
}

struct DenyEverything;

impl vibe_core::policy::ApprovalAgent for DenyEverything {
    fn request<'a>(
        &'a self,
        _request: vibe_core::policy::ApprovalRequest,
    ) -> vibe_core::policy::ApprovalFuture<'a> {
        Box::pin(async { Ok(vibe_core::policy::ApprovalDecision::Deny) })
    }
}

#[test]
fn a_completed_generation_never_outranks_a_failed_or_skipped_effect_state() {
    let failed = effect_of(&effect(
        "shell",
        json!({"command": "cargo test"}),
        json!({"status": "failed", "error": {"message": "exit 101"}}),
    ));
    assert_eq!(failed.indicator, Indicator::Error);
    assert_eq!(failed.header_text(), "Ran cargo test");
    assert_eq!(failed.body[0].text, "Error: exit 101");

    let skipped = effect_of(&effect(
        "shell",
        json!({"command": "rm -rf /"}),
        json!({"status": "skipped", "reason": "denied by the operator"}),
    ));
    assert_eq!(skipped.indicator, Indicator::Muted);
    assert_eq!(skipped.header_text(), "shell: skipped");
    assert_eq!(skipped.body[0].text, "Skipped: denied by the operator");

    let cancelled = effect_of(&effect(
        "shell",
        json!({"command": "sleep 60"}),
        json!({"status": "cancelled", "reason": "interrupted"}),
    ));
    assert_eq!(cancelled.indicator, Indicator::Muted);
    assert_eq!(cancelled.status, EntryStatus::Cancelled);
}

#[test]
fn in_flight_effects_expose_their_output_and_settle_without_it() {
    let entry = effect(
        "shell",
        json!({"command": "cargo build"}),
        json!({"status": "running", "outputText": "compiling\nlinking\n"}),
    );
    let running = effect_of(&entry);
    assert_eq!(running.indicator, Indicator::Running);
    assert_eq!(running_output(&entry), Some("compiling\nlinking\n"));
    assert!(running.body.is_empty());

    let pending = effect("shell", json!({}), json!({"status": "pending"}));
    assert_eq!(effect_of(&pending).status, EntryStatus::Pending);
    assert_eq!(running_output(&pending), None);

    let blocked = effect_of(&effect(
        "shell",
        json!({"command": "cargo build"}),
        json!({"status": "blocked", "callbackId": "callback-1", "outputText": ""}),
    ));
    assert_eq!(blocked.status, EntryStatus::Blocked);
    assert_eq!(blocked.indicator, Indicator::Running);
}

/// Reference `ProcessResultWidget`, one layout per operation.
#[test]
fn process_results_take_the_layout_their_operation_declares() {
    let texts = |output: Value| {
        process_lines(&output)
            .into_iter()
            .map(|line| line.text)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        texts(
            json!({"tool_name": "process.start", "processId": "p", "status": "running", "exitCode": null})
        ),
        ["processId: p", "status: running"]
    );
    assert_eq!(
        texts(
            json!({"tool_name": "process.output", "output": "", "bytesAvailable": 12, "hasMore": true, "truncatedBefore": true})
        ),
        [
            "(no output)",
            "12 bytes available | more output pending | earlier output truncated"
        ]
    );
    assert_eq!(
        texts(json!({"tool_name": "process.list", "processes": [
            {"processId": "0123456789abcdef", "status": "exited", "command": "make"},
            {"processId": "short"}
        ]})),
        ["89abcdef  exited  make", "short  ?  ?"]
    );
    assert_eq!(
        texts(json!({"tool_name": "process.list", "processes": []})),
        ["(no background processes)"]
    );
    assert_eq!(
        texts(json!({"tool_name": "process.write", "bytesWritten": 0, "status": ""})),
        ["bytesWritten: 0"]
    );
    assert!(texts(json!({"tool_name": "process.other"})).is_empty());
}

/// The rename moved the published names onto `read_file` and `grep`. Both
/// must project to the effect kind their predecessor did, and render the
/// same header from the reference argument keys, while the old names keep
/// working so a persisted transcript still replays.
#[test]
fn the_renamed_file_tools_render_exactly_as_their_predecessors_did() {
    assert_eq!(
        EffectKind::from_tool_name("read_file"),
        EffectKind::from_tool_name("read")
    );
    assert_eq!(
        EffectKind::from_tool_name("grep"),
        EffectKind::from_tool_name("search")
    );

    let output = json!({
        "status": "completed",
        "output": {
            "path": "src/lib.rs",
            "content": "1|use std;\n2|fn main() {}",
            "startLine": 1,
            "endLine": 2,
        },
    });
    let renamed = effect_of(&effect(
        "read_file",
        json!({"file_path": "src/lib.rs", "offset": 10}),
        output.clone(),
    ));
    let previous = effect_of(&effect(
        "read",
        json!({"path": "src/lib.rs", "offset": 10}),
        output,
    ));
    assert_eq!(renamed.header_text(), previous.header_text());
    assert_eq!(renamed.body[0].text, previous.body[0].text);
    assert_eq!(renamed.collapsed_by_default, previous.collapsed_by_default);
}

/// `path` as the reference displays a relative path: `str(Path)`, which
/// spells it with the host separator.
fn native(path: &str) -> String {
    path.split('/')
        .collect::<std::path::PathBuf>()
        .display()
        .to_string()
}

#[test]
fn every_audited_effect_kind_keeps_its_reference_header_body_and_collapse() {
    let read = effect_of(&effect(
        "read",
        json!({"path": "src/lib.rs", "offset": 10}),
        json!({
            "status": "completed",
            "output": {"path": "src/lib.rs", "content": "1|use std;\n2|fn main() {}", "startLine": 1, "endLine": 2},
        }),
    ));
    assert_eq!(
        read.header_text(),
        format!("Read 2 lines from {}", native("src/lib.rs"))
    );
    assert_eq!(read.body[0].text, "use std;");
    assert!(read.collapsed_by_default);

    let write = effect_of(&effect(
        "write",
        json!({"file_path": "docs/readme.md"}),
        json!({"status": "completed", "output": {"path": "docs/readme.md", "content": "hello"}}),
    ));
    assert_eq!(
        write.header_text(),
        format!("Created {}", native("docs/readme.md"))
    );
    assert!(!write.collapsed_by_default);

    let edit = effect_of(&effect(
        "edit",
        json!({"path": "src/lib.rs"}),
        json!({
            "status": "completed",
            "output": {"file": "src/lib.rs", "old_string": "old", "new_string": "new"},
        }),
    ));
    assert_eq!(
        edit.header_text(),
        format!("Edited {}", native("src/lib.rs"))
    );
    assert!(!edit.collapsed_by_default);
    assert_eq!(
        edit.body
            .iter()
            .map(|line| (line.text.as_str(), line.style))
            .collect::<Vec<_>>(),
        vec![("- old", BodyStyle::Removed), ("+ new", BodyStyle::Added)]
    );

    // US-247: the body is read off the occurrences the `edit` projection
    // publishes, each already widened to the line it sat on.
    let occurrence_edit = effect_of(&effect(
        "edit",
        json!({"file_path": "src/lib.rs"}),
        json!({
            "status": "completed",
            "output": {
                "file": "src/lib.rs",
                "old_string": "old",
                "new_string": "new",
                "occurrences": [
                    {"start_line": 3, "old_text": "let old = 1;", "new_text": "let new = 1;"},
                    {"start_line": 7, "old_text": "drop(old)", "new_text": "drop(new)"},
                ],
            },
        }),
    ));
    assert_eq!(
        occurrence_edit
            .body
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>(),
        vec![
            "   3 - let old = 1;",
            "   3 + let new = 1;",
            "⋯",
            "   7 - drop(old)",
            "   7 + drop(new)",
        ]
    );

    // Reference `EditResultWidget.__init__`: an edit that published no
    // occurrence diffs its anchor strings, anchored on no line.
    let empty_edit = effect_of(&effect(
        "edit",
        json!({"file_path": "src/lib.rs"}),
        json!({
            "status": "completed",
            "outputText": "nothing to replace",
            "output": {
                "file": "src/lib.rs",
                "old_string": "old",
                "new_string": "new",
                "occurrences": [],
            },
        }),
    ));
    assert_eq!(
        empty_edit
            .body
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>(),
        vec!["- old", "+ new"]
    );

    let search = effect_of(&effect(
        "grep",
        json!({"pattern": "todo"}),
        json!({
            "status": "completed",
            "output": {
                "matches": "src/lib.rs:4:// todo",
                "match_count": 1,
                "pattern": "todo",
                "was_truncated": false,
            },
        }),
    ));
    assert_eq!(search.header_text(), "Searched todo (1 match)");
    assert_eq!(search.body[0].text, "src/lib.rs:4:// todo");

    let workspace_search = effect_of(&effect(
        "search",
        json!({"pattern": "todo"}),
        json!({
            "status": "completed",
            "output": [{"path": "src/lib.rs", "line": 4, "text": "// todo"}],
        }),
    ));
    assert_eq!(workspace_search.header_text(), "Searched (1 match)");
    assert_eq!(workspace_search.body[0].text, "src/lib.rs:4:// todo");

    let todo = effect_of(&effect(
        "todo",
        json!({"action": "write", "todos": [{}, {}]}),
        json!({
            "status": "completed",
            "output": {"verb": "Updated", "total_count": 2, "todos": [
                {"status": "completed", "content": "ship"},
                {"status": "in_progress", "content": "review"},
            ]},
        }),
    ));
    assert_eq!(todo.header_text(), "Updated 2 todos");
    assert_eq!(
        todo.body
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>(),
        vec!["☐ review", "☑ ship"]
    );

    let question = effect_of(&effect(
        "ask_user_question",
        json!({"questions": [{"question": "Ship it?"}]}),
        json!({
            "status": "completed",
            "output": {"answers": [{"question": "Ship it?", "answer": "yes"}]},
        }),
    ));
    assert_eq!(question.header_text(), "Answered \"Ship it?\" → yes");
    assert!(!question.collapsed_by_default);
    assert!(question.body.is_empty());

    let web = effect_of(&effect(
        "web_search",
        json!({"query": "ratatui"}),
        json!({
            "status": "completed",
            "output": {"query": "ratatui", "answer": "", "sources": [{"title": "Ratatui", "url": "https://ratatui.rs"}]},
        }),
    ));
    assert_eq!(web.header_text(), "Searched 'ratatui' (1 source)");
    assert_eq!(web.body[0].text, "query: ratatui");

    let fetch = effect_of(&effect(
        "web_fetch",
        json!({"url": "https://example.com/page?a=1"}),
        json!({
            "status": "completed",
            "output": {
                "url": "https://example.com/page?a=1",
                "content": "body",
                "content_type": "text/html; charset=utf-8",
            },
        }),
    ));
    assert_eq!(
        fetch.header_text(),
        "Fetched https://example.com/page?a=1 (4 chars, text/html)"
    );
}

#[test]
fn only_a_manual_shell_command_breaks_the_tool_group() {
    let shell = effect("bash", json!({}), json!({"status": "running"}));
    let edit = effect("edit", json!({}), json!({"status": "running"}));
    let manual = effect(
        MANUAL_SHELL_TOOL_NAME,
        json!({}),
        json!({"status": "running"}),
    );
    assert!(keeps_tool_group(&shell));
    assert!(keeps_tool_group(&edit));
    assert!(!keeps_tool_group(&manual));

    let mut reasoning = shell.clone();
    reasoning.kind = TranscriptKind::Reasoning;
    assert!(keeps_tool_group(&reasoning));

    let mut assistant = shell;
    assistant.kind = TranscriptKind::AssistantMessage;
    assert!(!keeps_tool_group(&assistant));
}

#[test]
fn notices_split_into_hook_loop_and_severity_regions() {
    let notice = |detail: Value, level: &str| {
        crate::tui::hydration::published_fixture(
            "notice",
            json!({
                "type": "notice",
                "level": level,
                "message": "message body",
                "detail": detail,
            }),
        )
    };
    assert_eq!(
        region(&notice(
            json!({"kind": "hook_completed", "hookName": "format", "content": "reformatted", "status": "ok"}),
            "info"
        )),
        Region::Hook {
            icon: "✓",
            line: "[format] reformatted".to_owned()
        }
    );
    assert_eq!(
        region(&notice(
            json!({"kind": "scheduled_loop_fired", "loopId": "loop-1"}),
            "info"
        )),
        Region::Command {
            message: "message body".to_owned()
        }
    );
    // Every notice with no presentation of its own reads at the level the
    // server published it at.
    assert_eq!(
        region(&notice(json!({"kind": "plan_review_ended"}), "error")),
        Region::Notice {
            level: NoticeLevel::Error,
            message: "message body".to_owned()
        }
    );
}

#[test]
fn a_hook_run_draws_only_what_its_completions_say_beside_the_call() {
    let hook = |id: &str, detail: Value| {
        crate::tui::hydration::published_fixture(
            id,
            json!({"type": "notice", "level": "info", "message": "notice", "detail": detail}),
        )
    };
    let mut call = effect(
        "bash",
        json!({"command": "ls"}),
        json!({"status": "running"}),
    );
    call.id = "call_1".to_owned();
    let opened = hook(
        "opened",
        json!({"kind": "hook_run_started", "scope": "pre_tool", "toolCallId": "call_1"}),
    );
    let started = hook(
        "started",
        json!({"kind": "hook_started", "scope": "pre_tool", "toolCallId": "call_1", "hookName": "guard"}),
    );
    let quiet = hook(
        "quiet",
        json!({"kind": "hook_completed", "scope": "pre_tool", "toolCallId": "call_1", "hookName": "guard", "status": "ok"}),
    );
    let guard = hook(
        "guard",
        json!({"kind": "hook_completed", "scope": "pre_tool", "toolCallId": "call_1", "hookName": "guard", "status": "warning", "content": "checked"}),
    );
    let audit = hook(
        "audit",
        json!({"kind": "hook_completed", "scope": "post_tool", "toolCallId": "call_1", "hookName": "audit", "status": "ok", "content": "logged"}),
    );
    let review = hook(
        "review",
        json!({"kind": "hook_completed", "scope": "post_agent", "hookName": "review", "status": "error", "content": "retry"}),
    );
    let answer = crate::tui::hydration::published_fixture(
        "answer",
        json!({"type": "message", "role": "assistant", "content": [{"type": "text", "text": "done"}]}),
    );
    assert!(!is_drawn(&opened) && !is_drawn(&started) && !is_drawn(&quiet));
    assert!(is_drawn(&guard) && is_drawn(&call));

    assert_eq!(
        activity_status(&[call.clone(), started.clone()]),
        "Running hook guard"
    );
    assert_eq!(
        activity_status(&[call.clone(), started, quiet]),
        crate::tui::diagnostics::DEFAULT_ACTIVITY_STATUS
    );

    let placed = place_hook_lines(vec![&call, &answer, &guard, &audit, &review]);
    assert_eq!(
        placed
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        ["guard", "call_1", "audit", "answer", "review"]
    );
    assert_eq!(
        region(&review),
        Region::Hook {
            icon: "✗",
            line: "[review] retry".to_owned()
        }
    );
}

#[test]
fn unknown_tools_and_malformed_details_stay_visible_without_inventing_success() {
    let unknown = effect_of(&effect(
        "mcp__thing__do",
        json!({"target": "x", "count": 2}),
        json!({"status": "completed", "output": {"ok": true}}),
    ));
    assert_eq!(unknown.kind, EffectKind::Tool);
    assert_eq!(
        unknown.header_text(),
        "Ran mcp__thing__do(target='x', count=2)"
    );
    assert_eq!(unknown.body[0].text, "ok: true");

    // An effect entry with no canonical state behind it is presented from
    // its own text, and never invents a settled outcome.
    let restored = TranscriptEntry {
        id: "effect".to_owned(),
        revision: 1,
        kind: TranscriptKind::Effect,
        text: "broken".to_owned(),
        status: EntryStatus::Streaming,
        source: EntrySource::Restored,
    };
    let restored = effect_of(&restored);
    assert_eq!(restored.status, EntryStatus::Streaming);
    assert_eq!(restored.indicator, Indicator::Running);
    assert_eq!(restored.kind, EffectKind::Tool);
}

#[test]
fn an_effect_restored_without_its_state_keeps_its_settled_status_and_output() {
    let restored = TranscriptEntry {
        id: "persisted:session:3".to_owned(),
        revision: 1,
        kind: TranscriptKind::Effect,
        text: "Tool call-7\nexit status 1".to_owned(),
        status: EntryStatus::Failed,
        source: EntrySource::Restored,
    };
    let restored = effect_of(&restored);
    assert_eq!(restored.status, EntryStatus::Failed);
    assert_eq!(restored.indicator, Indicator::Error);
    assert_eq!(restored.header_text(), "Tool call-7");
    assert_eq!(restored.body[0].text, "exit status 1");
}

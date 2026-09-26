//! What a hook file loads, how a hook's answer is read, and what a chain
//! yields. The messages asserted here are the ones the pinned reference
//! printed for the same inputs (`scripts/parity/hooks.py` records them end to
//! end); these tests pin them at the unit where each one is made.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::json::decode_error;
use super::*;

fn write(directory: &Path, name: &str, text: &str) -> PathBuf {
    let path = directory.join(name);
    std::fs::write(&path, text).expect("the hook file is written");
    path
}

fn messages(result: &HookConfigResult) -> Vec<&str> {
    result
        .issues
        .iter()
        .map(|issue| issue.message.as_str())
        .collect()
}

fn hook(name: &str, hook_type: HookType, command: &str) -> HookConfig {
    HookConfig {
        name: name.to_owned(),
        hook_type,
        command: command.to_owned(),
        matcher: None,
        timeout: DEFAULT_HOOK_TIMEOUT,
        strict: false,
        description: None,
    }
}

fn pre_tool(tool_name: &str) -> HookInvocation {
    HookInvocation::PreTool {
        context: HookSessionContext {
            session_id: "session".to_owned(),
            cwd: ".".to_owned(),
            ..HookSessionContext::default()
        },
        tool_name: tool_name.to_owned(),
        tool_call_id: "call_1".to_owned(),
        tool_input: json!({"command": "echo hi"}),
    }
}

fn post_agent() -> HookInvocation {
    HookInvocation::PostAgent {
        context: HookSessionContext::default(),
    }
}

/// Everything a chain yields, in order.
async fn run(hooks: Vec<HookConfig>, invocation: HookInvocation) -> Vec<HookYield> {
    let manager = HooksManager::new(hooks, std::env::temp_dir());
    run_with(&manager, invocation).await
}

async fn run_with(manager: &HooksManager, invocation: HookInvocation) -> Vec<HookYield> {
    let mut yielded = Vec::new();
    manager
        .run(invocation, |item| {
            yielded.push(item);
            ControlFlow::Continue(())
        })
        .await;
    yielded
}

/// The `(status, content)` of every hook that completed.
fn completions(yielded: &[HookYield]) -> Vec<(HookSeverity, Option<String>)> {
    yielded
        .iter()
        .filter_map(|item| match item {
            HookYield::Event(HookEvent::Completed {
                status, content, ..
            }) => Some((*status, content.clone())),
            _ => None,
        })
        .collect()
}

fn answering(stdout: &Value) -> String {
    format!("printf '%s' '{stdout}'")
}

#[test]
fn a_declared_hook_loads_with_the_reference_defaults() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = write(
        directory.path(),
        "hooks.toml",
        "[[hooks]]\nname = \"guard\"\ntype = \"pre_tool\"\ncommand = \"true\"\n",
    );
    let result = load_hooks_file(&path);
    assert!(result.issues.is_empty(), "{:?}", result.issues);
    assert_eq!(result.hooks, [hook("guard", HookType::PreTool, "true")]);
}

#[test]
fn values_are_read_as_pydantic_reads_them_in_lax_mode() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = write(
        directory.path(),
        "hooks.toml",
        "[[hooks]]\nname = \"lax\"\ntype = \"post_tool\"\ncommand = \"true\"\n\
         timeout = \"1_5\"\nstrict = \"yes\"\nunknown = 1\n",
    );
    let result = load_hooks_file(&path);
    assert!(result.issues.is_empty(), "{:?}", result.issues);
    assert_eq!(result.hooks[0].timeout, 15.0);
    assert!(result.hooks[0].strict);
}

#[test]
fn every_invalid_entry_is_reported_under_its_name_and_the_rest_load() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = write(
        directory.path(),
        "hooks.toml",
        "[[hooks]]\ntype = \"pre_tool\"\ncommand = \"true\"\n\n\
         [[hooks]]\nname = 5\ntype = \"pre_tool\"\ncommand = \"true\"\n\n\
         [[hooks]]\nname = \"bad_type\"\ntype = \"pre_turn\"\ncommand = \"true\"\n\n\
         [[hooks]]\nname = \"empty_command\"\ntype = \"pre_tool\"\ncommand = \"  \"\n\n\
         [[hooks]]\nname = \"agent_match\"\ntype = \"post_agent\"\ncommand = \"true\"\nmatch = \"bash\"\n\n\
         [[hooks]]\nname = \"bad_timeout\"\ntype = \"pre_tool\"\ncommand = \"true\"\ntimeout = \"soon\"\n\n\
         [[hooks]]\nname = \"number_strict\"\ntype = \"pre_tool\"\ncommand = \"true\"\nstrict = 2\n\n\
         [[hooks]]\nname = \"many\"\ntype = 3\ncommand = []\ntimeout = []\n\n\
         [[hooks]]\nname = \"valid\"\ntype = \"pre_tool\"\ncommand = \"true\"\n\n\
         [[hooks]]\nname = \"valid\"\ntype = \"post_tool\"\ncommand = \"true\"\n",
    );
    let result = load_hooks_from_fs(&[path]);
    assert_eq!(
        messages(&result),
        [
            "hooks[0] - name: Field required",
            "5 - name: Input should be a valid string",
            "bad_type - type: Input should be 'post_agent', 'pre_tool' or 'post_tool'",
            "empty_command - command: Value error, command must not be empty",
            "agent_match - hook: Value error, match is only valid for tool hooks (pre_tool / \
             post_tool)",
            "bad_timeout - timeout: Input should be a valid number, unable to parse string as a \
             number",
            "number_strict - strict: Input should be a valid boolean, unable to interpret input",
            "many - type: Input should be 'post_agent', 'pre_tool' or 'post_tool' ; command: \
             Input should be a valid string ; timeout: Input should be a valid number",
            "Duplicate hook name: 'valid'",
        ]
    );
    assert_eq!(result.hooks.len(), 1);
    assert_eq!(result.hooks[0].hook_type, HookType::PreTool);
}

#[test]
fn a_file_that_is_not_a_hook_list_is_one_issue() {
    let directory = tempfile::tempdir().expect("a directory");
    let not_a_list = write(directory.path(), "list.toml", "hooks = 5\n");
    let not_tables = write(directory.path(), "tables.toml", "hooks = [1, \"two\"]\n");
    let not_toml = write(directory.path(), "toml.toml", "[[hooks]\nname = \"x\"\n");
    assert_eq!(
        messages(&load_hooks_file(&not_a_list)),
        ["hooks: Input should be a valid list"]
    );
    assert_eq!(
        messages(&load_hooks_file(&not_tables)),
        [
            "hooks[0] - hook: Input should be a valid dictionary or instance of HookConfig",
            "hooks[1] - hook: Input should be a valid dictionary or instance of HookConfig",
        ]
    );
    let unreadable = load_hooks_file(&not_toml);
    assert!(unreadable.hooks.is_empty());
    assert!(messages(&unreadable)[0].starts_with("Failed to parse: "));
    assert!(load_hooks_file(&directory.path().join("absent.toml")) == HookConfigResult::default());
}

#[test]
fn the_first_file_to_declare_a_name_keeps_it() {
    let directory = tempfile::tempdir().expect("a directory");
    let project = write(
        directory.path(),
        "project.toml",
        "[[hooks]]\nname = \"guard\"\ntype = \"pre_tool\"\ncommand = \"project\"\n",
    );
    let user = write(
        directory.path(),
        "user.toml",
        "[[hooks]]\nname = \"guard\"\ntype = \"pre_tool\"\ncommand = \"user\"\n\n\
         [[hooks]]\nname = \"mine\"\ntype = \"pre_tool\"\ncommand = \"true\"\n",
    );
    let result = load_hooks_from_fs(&[project, user.clone()]);
    assert_eq!(
        result
            .hooks
            .iter()
            .map(|hook| (hook.name.as_str(), hook.command.as_str()))
            .collect::<Vec<_>>(),
        [("guard", "project"), ("mine", "true")]
    );
    assert_eq!(result.issues.len(), 1);
    assert_eq!(result.issues[0].file, user);
    assert_eq!(result.issues[0].message, "Duplicate hook name: 'guard'");
}

#[test]
fn a_document_python_refuses_is_located_as_its_decoder_locates_it() {
    assert_eq!(decode_error("not json"), Some(("Expecting value", 1, 1)));
    assert_eq!(
        decode_error("{\"a\": 1} extra"),
        Some(("Extra data", 1, 10))
    );
    assert_eq!(
        decode_error("{\"decision\": \"allo"),
        Some(("Unterminated string starting at", 1, 14))
    );
    assert_eq!(
        decode_error("{\n  \"a\" 1}"),
        Some(("Expecting ':' delimiter", 2, 7))
    );
    assert_eq!(
        decode_error("{'a': 1}"),
        Some(("Expecting property name enclosed in double quotes", 1, 2))
    );
    assert_eq!(
        decode_error("[1 2]"),
        Some(("Expecting ',' delimiter", 1, 4))
    );
    assert_eq!(decode_error("\"\\x\""), Some(("Invalid \\escape", 1, 2)));
    assert_eq!(decode_error("NaN"), None);
    assert_eq!(decode_error(" {\"a\": [1, -2.5e3, true, null]} "), None);
}

#[test]
fn rewritten_arguments_are_written_as_python_json_dumps_writes_them() {
    let value: Value =
        serde_json::from_str(r#"{"command": "é \"x\"\n", "items": [1, null], "timeout": 1.0}"#)
            .expect("the value parses");
    assert_eq!(
        python_json_dumps(&value),
        r#"{"command": "\u00e9 \"x\"\n", "items": [1, null], "timeout": 1.0}"#
    );
    assert_eq!(python_json_dumps(&json!("😀")), r#""\ud83d\ude00""#);
}

#[test]
fn an_invocation_is_written_in_the_reference_field_order() {
    let text =
        String::from_utf8(pre_tool("bash").to_json().expect("it serializes")).expect("it is UTF-8");
    assert_eq!(
        text,
        r#"{"session_id":"session","transcript_path":"","cwd":".","parent_session_id":null,"hook_event_name":"pre_tool","tool_name":"bash","tool_call_id":"call_1","tool_input":{"command":"echo hi"}}"#
    );
}

#[tokio::test]
async fn a_pre_tool_denial_ends_the_chain_with_the_reason() {
    let deny = answering(&json!({"decision": "deny", "reason": "not today"}));
    let yielded = run(
        vec![
            hook("guard", HookType::PreTool, &deny),
            hook("after", HookType::PreTool, "true"),
        ],
        pre_tool("bash"),
    )
    .await;
    assert_eq!(
        completions(&yielded),
        [(HookSeverity::Error, Some("Denied tool 'bash'".to_owned()))]
    );
    assert!(yielded.contains(&HookYield::ToolDenial {
        hook_name: "guard".to_owned(),
        content: "not today".to_owned(),
    }));
    // The chain stops at the denial: the next hook never starts.
    assert!(!yielded.iter().any(|item| matches!(
        item,
        HookYield::Event(HookEvent::Started { hook_name, .. }) if hook_name == "after"
    )));
}

#[tokio::test]
async fn a_hook_only_runs_for_the_tools_it_matches() {
    let mut glob = hook("glob", HookType::PreTool, "true");
    glob.matcher = Some("BA?H".to_owned());
    let mut regex = hook("regex", HookType::PreTool, "true");
    regex.matcher = Some("re:^(grep|bash)$".to_owned());
    let mut other = hook("other", HookType::PreTool, "true");
    other.matcher = Some("read_file".to_owned());
    let yielded = run(vec![glob, regex, other], pre_tool("bash")).await;
    let started = yielded
        .iter()
        .filter_map(|item| match item {
            HookYield::Event(HookEvent::Started { hook_name, .. }) => Some(hook_name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(started, ["glob", "regex"]);
}

#[tokio::test]
async fn a_failed_hook_warns_with_its_most_telling_output_unless_it_is_strict() {
    let yielded = run(
        vec![
            hook(
                "stderr",
                HookType::PreTool,
                "echo out; echo '  broke  ' >&2; exit 1",
            ),
            hook("stdout", HookType::PreTool, "echo only stdout; exit 4"),
            hook("bare", HookType::PreTool, "exit 3"),
        ],
        pre_tool("bash"),
    )
    .await;
    assert_eq!(
        completions(&yielded),
        [
            (HookSeverity::Warning, Some("broke".to_owned())),
            (HookSeverity::Warning, Some("only stdout".to_owned())),
            (HookSeverity::Warning, Some("exited with code 3".to_owned())),
        ]
    );
    let mut strict = hook("strict", HookType::PreTool, "exit 2");
    strict.strict = true;
    let yielded = run(vec![strict], pre_tool("bash")).await;
    assert_eq!(
        completions(&yielded),
        [(
            HookSeverity::Error,
            Some("Denied tool 'bash' (strict)".to_owned())
        )]
    );
}

#[tokio::test]
async fn a_hook_past_its_timeout_is_stopped_and_reported() {
    let mut slow = hook("slow", HookType::PreTool, "sleep 5");
    slow.timeout = 0.2;
    let started = std::time::Instant::now();
    let yielded = run(vec![slow], pre_tool("bash")).await;
    assert!(started.elapsed() < std::time::Duration::from_secs(4));
    assert_eq!(
        completions(&yielded),
        [(
            HookSeverity::Warning,
            Some("Timed out after 0.2s".to_owned())
        )]
    );
}

#[tokio::test]
async fn an_answer_that_is_not_a_response_object_is_reported_as_python_reports_it() {
    let yielded = run(
        vec![
            hook("text", HookType::PreTool, "echo not json"),
            hook("list", HookType::PreTool, "echo '[1, 2]'"),
            hook(
                "decision",
                HookType::PreTool,
                &answering(&json!({"decision": "a".repeat(80)})),
            ),
        ],
        pre_tool("bash"),
    )
    .await;
    let invalid = |detail: &str| {
        (
            HookSeverity::Warning,
            Some(format!("invalid response: {detail}")),
        )
    };
    assert_eq!(
        completions(&yielded),
        [
            invalid("stdout was not valid JSON: Expecting value at line 1 col 1"),
            invalid("stdout was a JSON list, expected an object"),
            invalid(&format!(
                "stdout JSON did not match the hook response schema: 1 validation error for \
                 HookStructuredResponse\ndecision\n  Input should be 'allow' or 'deny' \
                 [type=literal_error, input_value='{}...{}', input_type=str]",
                "a".repeat(24),
                "a".repeat(23)
            )),
        ]
    );
}

#[tokio::test]
async fn a_rewrite_and_a_post_tool_change_are_yielded_for_the_loop_to_apply() {
    let rewrite = answering(&json!({"hook_specific_output": {"tool_input": {"command": "ls"}}}));
    let yielded = run(
        vec![hook("rewriter", HookType::PreTool, &rewrite)],
        pre_tool("bash"),
    )
    .await;
    assert!(yielded.contains(&HookYield::ToolInputRewrite {
        hook_name: "rewriter".to_owned(),
        tool_input: json!({"command": "ls"}),
    }));
    assert_eq!(
        completions(&yielded),
        [(
            HookSeverity::Warning,
            Some("Rewrote tool_input for 'bash'".to_owned())
        )]
    );

    let replace = answering(&json!({"decision": "deny", "reason": "redacted"}));
    let append = answering(&json!({"hook_specific_output": {"additional_context": "noted"}}));
    let invocation = HookInvocation::PostTool {
        context: HookSessionContext::default(),
        tool_name: "bash".to_owned(),
        tool_call_id: "call_1".to_owned(),
        tool_input: json!({}),
        tool_status: ToolStatus::Success,
        tool_output: None,
        tool_output_text: "secret".to_owned(),
        tool_error: None,
        duration_ms: 1.0,
    };
    let yielded = run(
        vec![
            hook("redact", HookType::PostTool, &replace),
            hook("note", HookType::PostTool, &append),
        ],
        invocation,
    )
    .await;
    let texts = yielded
        .iter()
        .filter_map(|item| match item {
            HookYield::TextReplacement(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, ["redacted", "redacted\nnoted"]);
}

#[tokio::test]
async fn a_post_agent_denial_retries_three_times_per_user_turn() {
    let deny = answering(&json!({"decision": "deny", "reason": "again"}));
    let manager = HooksManager::new(
        vec![hook("review", HookType::PostAgent, &deny)],
        std::env::temp_dir(),
    );
    let mut statuses = Vec::new();
    for _ in 0..=MAX_RETRIES {
        let yielded = run_with(&manager, post_agent()).await;
        statuses.extend(
            completions(&yielded)
                .into_iter()
                .map(|(_, content)| content),
        );
        let retried = yielded.contains(&HookYield::UserMessage("again".to_owned()));
        assert_eq!(retried, statuses.len() <= MAX_RETRIES as usize);
    }
    assert_eq!(
        statuses,
        [
            Some("Failed, retrying (3 retries remaining)".to_owned()),
            Some("Failed, retrying (2 retries remaining)".to_owned()),
            Some("Failed, retrying (1 retry remaining)".to_owned()),
            Some("Failed, retries exhausted (3/3)".to_owned()),
        ]
    );
    manager.reset_retry_count();
    let yielded = run_with(&manager, post_agent()).await;
    assert!(yielded.contains(&HookYield::UserMessage("again".to_owned())));
}

//! What the shell family publishes, refuses, runs and cleans up.
//!
//! Every case here drives the tools through [`ToolRegistry::invoke`], which is
//! the path a model call takes: schema validation and default application
//! happen there, so a test that called a handler directly would prove less than
//! it appears to.

use std::sync::atomic::{AtomicUsize, Ordering};

use tempfile::{TempDir, tempdir};

use super::*;
use crate::policy::{
    ApprovalDecision, ApprovalFuture, ApprovalRequest, TrustDecision, TrustRootKind,
};

/// Answers every approval the same way and records what it was asked to
/// approve: the tool, and the label of every requirement the call carried.
struct ScriptedApproval {
    decision: ApprovalDecision,
    requests: Arc<StdMutex<Vec<String>>>,
}

impl ScriptedApproval {
    fn new(decision: ApprovalDecision) -> (Arc<Self>, Arc<StdMutex<Vec<String>>>) {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        (
            Arc::new(Self {
                decision,
                requests: requests.clone(),
            }),
            requests,
        )
    }
}

impl ApprovalAgent for ScriptedApproval {
    fn request<'a>(&'a self, request: ApprovalRequest) -> ApprovalFuture<'a> {
        if let Ok(mut requests) = self.requests.lock() {
            let labels = request
                .requirements
                .iter()
                .map(PermissionRequirement::label)
                .collect::<Vec<_>>()
                .join("; ");
            requests.push(format!("{}: {labels}", request.tool));
        }
        let decision = self.decision;
        Box::pin(async move { Ok(decision) })
    }
}

struct Harness {
    directory: TempDir,
    registry: ToolRegistry,
    tools: ShellTools,
    requests: Arc<StdMutex<Vec<String>>>,
}

impl Harness {
    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn shell(&self) -> Arc<SessionShell> {
        self.tools
            .session_shell("session-1")
            .expect("session shell")
    }

    fn approval_count(&self) -> usize {
        self.requests.lock().map_or(0, |requests| requests.len())
    }

    /// Everything the operator was asked to approve, tool and requirement
    /// labels together.
    fn approvals(&self) -> String {
        self.requests
            .lock()
            .map_or_else(|_| String::new(), |requests| requests.join(" | "))
    }

    async fn call(&self, tool: &str, arguments: Value) -> Result<ToolExecutionOutput, ToolError> {
        self.registry
            .invoke(
                tool,
                ToolInvocation {
                    call_id: format!("{tool}-1"),
                    arguments,
                },
            )
            .await
    }

    fn schema(&self, tool: &str) -> Value {
        self.registry
            .list()
            .expect("list")
            .into_iter()
            .find(|spec| spec.name == tool)
            .expect("the tool is published")
            .input_schema
    }

    fn names(&self) -> Vec<String> {
        self.registry
            .list()
            .expect("list")
            .into_iter()
            .map(|spec| spec.name)
            .collect()
    }
}

async fn harness(rollout: ShellRollout, decision: ApprovalDecision) -> Harness {
    let directory = tempdir().expect("tempdir");
    let policy = PermissionStore::default();
    policy
        .set_trust(
            directory.path(),
            TrustDecision::Trusted,
            TrustRootKind::Workspace,
        )
        .await
        .expect("trust");
    let (approval, requests) = ScriptedApproval::new(decision);
    let registry = ToolRegistry::default();
    let tools = ShellTools::new(directory.path().join("home"), rollout);
    tools
        .register(
            "session-1",
            directory.path(),
            &registry,
            policy,
            approval as Arc<dyn ApprovalAgent>,
        )
        .expect("the shell family registers");
    Harness {
        directory,
        registry,
        tools,
        requests,
    }
}

// --------------------------------------------------------------------------
// Published surface
// --------------------------------------------------------------------------

/// The legacy variant, property for property, as the reference emits it.
#[tokio::test]
async fn the_legacy_bash_schema_carries_the_two_reference_properties() {
    let harness = harness(ShellRollout::Legacy, ApprovalDecision::Deny).await;
    let schema = harness.schema("bash");
    assert_eq!(schema["required"], json!(["command"]));
    assert_eq!(schema["properties"]["command"]["type"], json!("string"));
    assert_eq!(
        schema["properties"]["timeout"]["anyOf"],
        json!([{"type": "integer"}, {"type": "null"}])
    );
    assert_eq!(schema["properties"]["timeout"]["default"], Value::Null);
    let properties = schema["properties"]
        .as_object()
        .expect("properties")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(properties, ["command", "timeout"]);
    assert!(
        schema.get("additionalProperties").is_none(),
        "the reference model does not forbid extra keys"
    );
}

/// The managed variant replaces the legacy one and publishes the eight
/// reference properties.
#[tokio::test]
async fn the_managed_rollout_publishes_the_eight_property_bash_and_its_four_session_tools() {
    let legacy = harness(ShellRollout::Legacy, ApprovalDecision::Deny).await;
    assert_eq!(legacy.names(), ["bash"]);

    let managed = harness(ShellRollout::Managed, ApprovalDecision::Deny).await;
    assert_eq!(
        managed.names(),
        [
            "bash",
            "bash_log_file",
            "bash_output",
            "bash_sessions",
            "bash_stdin"
        ]
    );
    let schema = managed.schema("bash");
    let mut properties = schema["properties"]
        .as_object()
        .expect("properties")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    properties.sort();
    assert_eq!(
        properties,
        [
            "background",
            "command",
            "cwd",
            "env",
            "hard_timeout",
            "shell",
            "timeout",
            "timeout_seconds"
        ]
    );
    assert_eq!(
        schema["properties"]["env"]["anyOf"],
        json!([
            {"additionalProperties": {"type": "string"}, "type": "object"},
            {"type": "null"}
        ])
    );
    assert_eq!(
        schema["properties"]["timeout_seconds"]["anyOf"],
        json!([{"minimum": 0, "type": "number"}, {"type": "null"}])
    );
}

/// Both variants are available on a non-Windows host under the managed
/// rollout, so the published `bash` is decided by selection priority alone.
#[test]
fn the_managed_variant_outranks_the_legacy_one_whatever_the_registration_order() {
    for reversed in [false, true] {
        let registry = ToolRegistry::default();
        let mut specs = vec![bash_spec(false), bash_spec(true)];
        if reversed {
            specs.reverse();
        }
        for spec in specs {
            registry
                .register(spec, Arc::new(UnreachableHandler))
                .expect("register");
        }
        let published = registry
            .list()
            .expect("list")
            .into_iter()
            .find(|spec| spec.name == "bash")
            .expect("bash is published");
        assert_eq!(published.selection_priority, MANAGED_SELECTION_PRIORITY);
        assert!(
            published.input_schema["properties"]
                .get("background")
                .is_some(),
            "the managed schema must win the name"
        );
    }
}

struct UnreachableHandler;

impl ToolHandler for UnreachableHandler {
    fn invoke<'a>(
        &'a self,
        _invocation: &'a ToolInvocation,
        _output: ToolOutputSink,
    ) -> ToolHandlerFuture<'a> {
        Box::pin(async { Err(ToolError::Unavailable("never invoked".to_owned())) })
    }
}

/// The four session schemas, at the points where the reference shape is easy
/// to get wrong: a required key that is not annotated, an array without a
/// default, and an object with no required key at all.
#[tokio::test]
async fn the_session_tool_schemas_match_the_reference_shape() {
    let harness = harness(ShellRollout::Managed, ApprovalDecision::Deny).await;

    let output = harness.schema("bash_output");
    assert_eq!(output["required"], json!(["session_id"]));
    assert_eq!(
        output["properties"]["session_id"],
        json!({"type": "string"})
    );
    assert_eq!(
        output["properties"]["wait_seconds"],
        json!({"default": 0, "minimum": 0, "type": "number"})
    );
    assert_eq!(
        output["properties"]["max_bytes"]["anyOf"],
        json!([{"exclusiveMinimum": 0, "type": "integer"}, {"type": "null"}])
    );

    let stdin = harness.schema("bash_stdin");
    assert!(
        stdin["properties"]["control"].get("default").is_none(),
        "the reference builds `control` with a default factory, which emits no default"
    );
    assert_eq!(
        stdin["properties"]["control"]["items"]["enum"]
            .as_array()
            .expect("enum")
            .len(),
        CONTROL_KEYS.len()
    );

    let sessions = harness.schema("bash_sessions");
    assert!(
        sessions.get("required").is_none(),
        "no `bash_sessions` property is required"
    );
    assert_eq!(sessions["properties"]["action"]["default"], json!("list"));

    let log_file = harness.schema("bash_log_file");
    assert_eq!(log_file["required"], json!(["action"]));
    assert_eq!(
        log_file["properties"]["action"],
        json!({"enum": ["read", "write", "append"], "type": "string"})
    );
    assert_eq!(log_file["properties"]["offset"]["default"], json!(0));
}

// --------------------------------------------------------------------------
// Policy
// --------------------------------------------------------------------------

/// A command the analyser permits outright runs without an approval round
/// trip, matching the reference `ALWAYS` permission context.
#[tokio::test]
async fn an_allowlisted_command_runs_without_asking() {
    let harness = harness(ShellRollout::Legacy, ApprovalDecision::Deny).await;
    let output = harness
        .call("bash", json!({"command": "pwd"}))
        .await
        .expect("an allowlisted command needs no approval");
    assert!(!output.model_text.is_empty(), "{output:?}");
    assert_eq!(harness.approval_count(), 0);
}

/// A command the analyser does not permit outright is held until the operator
/// answers, and a refusal means no process ever started.
#[tokio::test]
async fn a_command_needing_approval_never_runs_when_it_is_refused() {
    let harness = harness(ShellRollout::Legacy, ApprovalDecision::Deny).await;
    let marker = harness.root().join("written-by-shell");
    let refused = harness
        .call(
            "bash",
            json!({"command": format!("echo hi > {}", marker.display())}),
        )
        .await
        .expect_err("a redirection is not allowlisted");
    assert!(refused.to_string().contains("denied"), "{refused}");
    assert_eq!(harness.approval_count(), 1);
    assert!(
        !marker.exists(),
        "approval must be resolved before the command runs"
    );
}

/// A destructive command is refused by the analysis itself: no approval is
/// raised, because no answer could make it run.
#[tokio::test]
async fn a_destructive_command_is_refused_before_any_approval() {
    let harness = harness(ShellRollout::Legacy, ApprovalDecision::ApproveOnce).await;
    let refused = harness
        .call("bash", json!({"command": "rm -rf /tmp/anything"}))
        .await
        .expect_err("a destructive command is denied");
    assert!(refused.to_string().contains("refused"), "{refused}");
    assert_eq!(harness.approval_count(), 0);
}

/// The managed variant runs more than the command text: `cwd`, `shell` and
/// `env` each decide part of what happens, and none of them is visible to an
/// analysis of the text. An allowlisted command carrying one must therefore
/// reach the operator, naming the override, rather than running outright.
#[tokio::test]
async fn a_managed_override_stops_an_allowlisted_command_from_running_outright() {
    for (arguments, named) in [
        (json!({"command": "pwd", "cwd": "/tmp"}), "outside workdir"),
        (
            json!({"command": "pwd", "shell": "/usr/bin/python3"}),
            "shell override: /usr/bin/python3",
        ),
        (
            json!({"command": "pwd", "env": {"LD_PRELOAD": "/tmp/hijack.so"}}),
            "env override: LD_PRELOAD",
        ),
    ] {
        let harness = harness(ShellRollout::Managed, ApprovalDecision::Deny).await;
        let refused = harness
            .call("bash", arguments.clone())
            .await
            .expect_err("an override is not allowlisted");
        assert!(refused.to_string().contains("denied"), "{refused}");
        assert_eq!(harness.approval_count(), 1, "{arguments}");
        assert!(
            harness.approvals().contains(named),
            "the approval must name the override: {}",
            harness.approvals()
        );
    }
}

/// The escalation is bound to what the override actually changes: a working
/// directory inside the session root is where the command would have run
/// anyway, so it is not a reason to ask.
#[tokio::test]
async fn a_working_directory_inside_the_session_root_does_not_ask() {
    let harness = harness(ShellRollout::Managed, ApprovalDecision::Deny).await;
    let inside = harness.root().join("inside");
    std::fs::create_dir_all(&inside).expect("inside");
    harness
        .call(
            "bash",
            json!({"command": "pwd", "cwd": inside.to_string_lossy()}),
        )
        .await
        .expect("an allowlisted command in the session root needs no approval");
    assert_eq!(harness.approval_count(), 0);
}

/// The legacy variant publishes none of the overrides and honours none of
/// them, so a hallucinated key changes neither where it runs nor whether it
/// asks.
#[tokio::test]
async fn the_legacy_variant_ignores_the_managed_overrides() {
    let harness = harness(ShellRollout::Legacy, ApprovalDecision::Deny).await;
    let output = harness
        .call("bash", json!({"command": "pwd", "cwd": "/tmp"}))
        .await
        .expect("the legacy variant ignores an override it does not publish");
    assert_eq!(harness.approval_count(), 0);
    let root = harness.root().canonicalize().expect("canonical root");
    assert_eq!(
        output.model_text.trim(),
        root.to_string_lossy(),
        "the command ran in the session root, not in the requested override"
    );
}

// --------------------------------------------------------------------------
// Legacy execution
// --------------------------------------------------------------------------

/// A non-zero exit carries the status and both streams, not an opaque failure.
#[tokio::test]
async fn a_failing_command_reports_its_status_and_its_output() {
    let harness = harness(ShellRollout::Legacy, ApprovalDecision::ApproveOnce).await;
    let failed = harness
        .call("bash", json!({"command": "echo out; echo err >&2; exit 3"}))
        .await
        .expect_err("a non-zero exit is a failure");
    let message = failed.to_string();
    assert!(message.contains("exit status 3"), "{message}");
    assert!(message.contains("out"), "{message}");
    assert!(message.contains("err"), "{message}");
}

/// Output past the tool limit is cut inside the sink contract and the cut is
/// stated to the model rather than left silent.
#[tokio::test]
async fn a_flood_of_output_is_bounded_and_the_truncation_is_reported() {
    let harness = harness(ShellRollout::Legacy, ApprovalDecision::ApproveOnce).await;
    let output = harness
        .call(
            "bash",
            json!({"command": "head -c 200000 /dev/zero | tr '\\0' 'z'"}),
        )
        .await
        .expect("a chatty command still succeeds");
    assert_eq!(output.typed_result["truncated"], json!(true));
    assert!(
        output.typed_result["stdout"]
            .as_str()
            .expect("stdout")
            .len()
            <= MAX_OUTPUT_BYTES,
        "the captured stream stays inside the reference limit"
    );
    assert!(
        output.model_text.contains("output truncated at"),
        "{}",
        output.model_text
    );
}

/// A command that outlives its timeout is killed with its process group, and
/// the terminal is released rather than left in the manager.
#[tokio::test]
async fn a_command_past_its_timeout_is_terminated_with_its_process_group() {
    let harness = harness(ShellRollout::Legacy, ApprovalDecision::ApproveOnce).await;
    let marker = harness.root().join("after-the-timeout");
    let timed_out = harness
        .call(
            "bash",
            json!({
                "command": format!("sleep 30; touch {}", marker.display()),
                "timeout": 1,
            }),
        )
        .await
        .expect_err("a command past its timeout fails");
    assert!(timed_out.to_string().contains("timed out"), "{timed_out}");
    assert!(
        harness.shell().terminals.list().await.is_empty(),
        "the terminal must be released"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!marker.exists(), "the process group must be gone");
}

/// A cancelled turn drops the tool future, and the guard that owns the
/// terminal terminates the process group on the way out.
#[tokio::test]
async fn a_cancelled_turn_terminates_the_process_group() {
    let harness = harness(ShellRollout::Legacy, ApprovalDecision::ApproveOnce).await;
    let marker = harness.root().join("after-the-cancel");
    let invocation = ToolInvocation {
        call_id: "bash-1".to_owned(),
        arguments: json!({"command": format!("sleep 2; touch {}", marker.display())}),
    };
    // Dropping the future is what a cancelled turn does: the engine's tool
    // select arm drops every pending call.
    let dropped = tokio::time::timeout(
        Duration::from_millis(400),
        harness.registry.invoke("bash", invocation),
    )
    .await;
    assert!(dropped.is_err(), "the call must still be running");

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !marker.exists(),
        "the command must not survive the turn that started it"
    );
    assert!(
        harness.shell().terminals.list().await.is_empty(),
        "the terminal must be released"
    );
}

// --------------------------------------------------------------------------
// Managed sessions
// --------------------------------------------------------------------------

async fn background_session(harness: &Harness, command: &str) -> String {
    let started = harness
        .call("bash", json!({"command": command, "background": true}))
        .await
        .expect("a background session starts");
    started.typed_result["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned()
}

/// A cursor read returns only what is new, and the reported cursor is where the
/// next read starts.
#[tokio::test]
async fn a_cursor_read_returns_only_the_bytes_that_followed_it() {
    let harness = harness(ShellRollout::Managed, ApprovalDecision::ApproveOnce).await;
    let session = background_session(&harness, "echo first; sleep 1; echo second").await;

    let first = harness
        .call(
            "bash_output",
            json!({"session_id": session, "wait_seconds": 5}),
        )
        .await
        .expect("the first poll answers");
    assert!(
        first.typed_result["output"]
            .as_str()
            .expect("output")
            .contains("first"),
        "{first:?}"
    );
    let cursor = first.typed_result["nextCursor"].as_u64().expect("cursor");

    let second = harness
        .call(
            "bash_output",
            json!({"session_id": session, "cursor": cursor, "wait_seconds": 5}),
        )
        .await
        .expect("the second poll answers");
    let text = second.typed_result["output"].as_str().expect("output");
    assert!(text.contains("second"), "{second:?}");
    assert!(!text.contains("first"), "{second:?}");
    assert!(second.typed_result["nextCursor"].as_u64().expect("cursor") > cursor);
}

/// A session that has exited still answers, with its last output and its exit
/// status rather than an error.
#[tokio::test]
async fn an_exited_session_still_reports_its_output_and_status() {
    let harness = harness(ShellRollout::Managed, ApprovalDecision::ApproveOnce).await;
    let session = background_session(&harness, "echo done; exit 7").await;
    let mut polled = harness
        .call(
            "bash_output",
            json!({"session_id": session, "wait_seconds": 5}),
        )
        .await
        .expect("poll");
    // The pump settles the status shortly after the process exits.
    for _ in 0..40 {
        if polled.typed_result["status"] != json!("running") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        polled = harness
            .call("bash_output", json!({"session_id": session}))
            .await
            .expect("poll");
    }
    assert_eq!(polled.typed_result["status"], json!("completed"));
    assert_eq!(polled.typed_result["exitCode"], json!(7));
    assert!(
        polled.typed_result["output"]
            .as_str()
            .expect("output")
            .contains("done"),
        "{polled:?}"
    );
}

/// An unknown session id answers with the ids that do exist, so a model that
/// guessed can correct itself without another round trip.
#[tokio::test]
async fn an_unknown_session_id_lists_the_active_sessions() {
    let harness = harness(ShellRollout::Managed, ApprovalDecision::ApproveOnce).await;
    let session = background_session(&harness, "sleep 5").await;
    for tool in ["bash_output", "bash_stdin"] {
        let unknown = harness
            .call(tool, json!({"session_id": "bash_absent", "text": "noop\n"}))
            .await
            .expect_err("an unknown session is refused");
        assert!(unknown.to_string().contains("bash_absent"), "{unknown}");
        assert!(unknown.to_string().contains(&session), "{unknown}");
    }
}

/// Text reaches the process, and the session that reads it says so.
#[tokio::test]
async fn text_written_to_a_session_reaches_the_process() {
    let harness = harness(ShellRollout::Managed, ApprovalDecision::ApproveOnce).await;
    let session = background_session(&harness, "read line; echo \"got $line\"").await;
    harness
        .call(
            "bash_stdin",
            json!({"session_id": session, "text": "ping\n"}),
        )
        .await
        .expect("stdin is written");
    let polled = harness
        .call(
            "bash_output",
            json!({"session_id": session, "wait_seconds": 5}),
        )
        .await
        .expect("poll");
    assert!(
        polled.typed_result["output"]
            .as_str()
            .expect("output")
            .contains("got ping"),
        "{polled:?}"
    );
}

/// The reference model accepts exactly one input, so nothing is resolved by
/// precedence and a malformed payload never reaches the process.
#[tokio::test]
async fn stdin_takes_exactly_one_input_and_refuses_bad_base64_before_writing() {
    let harness = harness(ShellRollout::Managed, ApprovalDecision::ApproveOnce).await;
    let session = background_session(&harness, "cat > /dev/null").await;
    let log = harness
        .shell()
        .managed
        .lock()
        .await
        .get(&session)
        .expect("session")
        .log_path
        .clone();

    let both = harness
        .call(
            "bash_stdin",
            json!({"session_id": session, "text": "a", "bytes_base64": "YQ=="}),
        )
        .await
        .expect_err("two inputs are refused");
    assert!(both.to_string().contains("exactly one"), "{both}");

    let neither = harness
        .call("bash_stdin", json!({"session_id": session}))
        .await
        .expect_err("no input is refused");
    assert!(neither.to_string().contains("exactly one"), "{neither}");

    let malformed = harness
        .call(
            "bash_stdin",
            json!({"session_id": "bash_absent", "bytes_base64": "not base64!!"}),
        )
        .await
        .expect_err("invalid base64 is refused");
    assert!(malformed.to_string().contains("base64"), "{malformed}");
    // The unknown session id proves the decode ran first: the session lookup
    // never happened, so nothing could have been written.
    assert!(
        !malformed.to_string().contains("active sessions"),
        "{malformed}"
    );

    harness
        .call(
            "bash_stdin",
            json!({"session_id": session, "control": ["ctrl_d"]}),
        )
        .await
        .expect("a control key is written");
    assert!(log.exists());
}

/// A session belongs to the Vibe session, not to the turn that started it, so
/// a later call can list, inspect and stop it.
#[tokio::test]
async fn a_session_started_by_one_call_is_listed_inspected_and_killed_by_another() {
    let harness = harness(ShellRollout::Managed, ApprovalDecision::ApproveOnce).await;
    let session = background_session(&harness, "sleep 30").await;

    let listed = harness
        .call("bash_sessions", json!({}))
        .await
        .expect("list");
    assert_eq!(listed.typed_result["action"], json!("list"));
    assert_eq!(
        listed.typed_result["sessions"][0]["sessionId"],
        json!(session)
    );

    let inspected = harness
        .call(
            "bash_sessions",
            json!({"action": "inspect", "session_id": session}),
        )
        .await
        .expect("inspect");
    assert_eq!(
        inspected.typed_result["session"]["status"],
        json!("running")
    );

    let killed = harness
        .call(
            "bash_sessions",
            json!({"action": "kill", "session_id": session}),
        )
        .await
        .expect("kill");
    assert_eq!(killed.typed_result["session"]["status"], json!("killed"));
    let after = harness
        .call("bash_sessions", json!({"action": "list"}))
        .await
        .expect("list");
    assert_eq!(after.typed_result["sessions"], json!([]));
}

/// `inspect` and `kill` name the session they need rather than guessing one.
#[tokio::test]
async fn inspect_and_kill_require_a_session_id() {
    let harness = harness(ShellRollout::Managed, ApprovalDecision::ApproveOnce).await;
    for action in ["inspect", "kill"] {
        let refused = harness
            .call("bash_sessions", json!({"action": action}))
            .await
            .expect_err("the action needs a session id");
        assert!(refused.to_string().contains("session_id"), "{refused}");
    }
}

/// `reset` stops every session, and `clear_logs` also removes what they wrote.
#[tokio::test]
async fn reset_stops_every_session_and_can_clear_their_logs() {
    let harness = harness(ShellRollout::Managed, ApprovalDecision::ApproveOnce).await;
    let session = background_session(&harness, "sleep 30").await;
    let log = harness
        .shell()
        .managed
        .lock()
        .await
        .get(&session)
        .expect("session")
        .log_path
        .clone();
    assert!(log.exists());

    let reset = harness
        .call(
            "bash_sessions",
            json!({"action": "reset", "clear_logs": true}),
        )
        .await
        .expect("reset");
    assert_eq!(reset.typed_result["sessions"][0]["status"], json!("killed"));
    assert!(!log.exists(), "clear_logs removes the stored logs");
}

// --------------------------------------------------------------------------
// Log files
// --------------------------------------------------------------------------

/// A session id reads that session's own log; a relative path that climbs out
/// of the log directory is refused before the filesystem is touched.
#[tokio::test]
async fn a_log_path_that_escapes_the_session_directory_is_refused() {
    let harness = harness(ShellRollout::Managed, ApprovalDecision::ApproveOnce).await;
    let session = background_session(&harness, "echo logged; sleep 30").await;
    harness
        .call(
            "bash_output",
            json!({"session_id": session, "wait_seconds": 5}),
        )
        .await
        .expect("poll");

    let read = harness
        .call(
            "bash_log_file",
            json!({"action": "read", "session_id": session}),
        )
        .await
        .expect("the session log reads back");
    assert!(
        read.typed_result["content"]
            .as_str()
            .expect("content")
            .contains("logged"),
        "{read:?}"
    );

    let secret = harness.root().join("secret");
    std::fs::write(&secret, "not for the model").expect("secret");
    let escaped = harness
        .call(
            "bash_log_file",
            json!({"action": "read", "relative_path": "../../secret"}),
        )
        .await
        .expect_err("an escaping path is refused");
    assert!(escaped.to_string().contains("escapes"), "{escaped}");

    let foreign = harness
        .call(
            "bash_log_file",
            json!({"action": "read", "relative_path": "sessions/powershell_1.log"}),
        )
        .await
        .expect_err("another family's session log is refused");
    assert!(foreign.to_string().contains("session file"), "{foreign}");

    // A session id names a file, so it is held to the same rule: it cannot
    // carry a path of its own.
    let climbing = harness
        .call(
            "bash_log_file",
            json!({"action": "read", "session_id": "../../../etc/passwd"}),
        )
        .await
        .expect_err("a session id carrying a path is refused");
    assert!(climbing.to_string().contains("session file"), "{climbing}");
}

/// A refusal names what the analyser objected to, so the model can propose
/// something else instead of retrying the same command.
#[tokio::test]
async fn a_refused_command_carries_the_analysis_rationale() {
    let harness = harness(ShellRollout::Legacy, ApprovalDecision::ApproveOnce).await;
    let refused = harness
        .call("bash", json!({"command": "dd if=/dev/zero of=/dev/sda"}))
        .await
        .expect_err("a destructive command is denied");
    assert!(refused.to_string().contains("destructive"), "{refused}");
    assert!(refused.to_string().contains("dd"), "{refused}");
}

/// A live session's log is fed by its process, so writing to it is refused;
/// any other file under the shell-tool directory is writable.
#[tokio::test]
async fn a_live_session_log_cannot_be_written_but_a_scratch_file_can() {
    let harness = harness(ShellRollout::Managed, ApprovalDecision::ApproveOnce).await;
    let session = background_session(&harness, "sleep 30").await;

    let refused = harness
        .call(
            "bash_log_file",
            json!({"action": "write", "session_id": session, "content": "x"}),
        )
        .await
        .expect_err("a live session log is not writable");
    assert!(refused.to_string().contains("live session"), "{refused}");

    harness
        .call(
            "bash_log_file",
            json!({"action": "write", "relative_path": "notes.txt", "content": "one"}),
        )
        .await
        .expect("write");
    harness
        .call(
            "bash_log_file",
            json!({"action": "append", "relative_path": "notes.txt", "content": "two"}),
        )
        .await
        .expect("append");
    let read = harness
        .call(
            "bash_log_file",
            json!({"action": "read", "relative_path": "notes.txt"}),
        )
        .await
        .expect("read");
    assert_eq!(read.typed_result["content"], json!("onetwo"));
}

// --------------------------------------------------------------------------
// Reporting and teardown
// --------------------------------------------------------------------------

/// A session whose buffer overflowed says so, rather than reporting a shorter
/// output as if it were complete.
#[tokio::test]
async fn a_dropped_output_chunk_is_reported_to_the_model() {
    let harness = harness(ShellRollout::Managed, ApprovalDecision::ApproveOnce).await;
    let session = background_session(&harness, "sleep 30").await;
    let managed = harness
        .shell()
        .managed
        .lock()
        .await
        .get(&session)
        .expect("session")
        .clone();
    append_chunks(&managed, &[], true);

    let polled = harness
        .call("bash_output", json!({"session_id": session}))
        .await
        .expect("poll");
    assert_eq!(polled.typed_result["backpressureDropped"], json!(true));
    assert!(polled.model_text.contains("dropped"), "{polled:?}");
}

/// Closing the Vibe session stops what it left running: a managed session
/// outlives its call by design, so nothing else can.
#[tokio::test]
async fn closing_the_session_terminates_every_session_it_left_running() {
    let harness = harness(ShellRollout::Managed, ApprovalDecision::ApproveOnce).await;
    let marker = harness.root().join("after-the-close");
    background_session(&harness, &format!("sleep 2; touch {}", marker.display())).await;

    harness
        .tools
        .close_session("session-1")
        .await
        .expect("the session closes");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !marker.exists(),
        "a managed session must not outlive its Vibe session"
    );
}

/// The reference clamps the foreground wait; a request past the ceiling lands
/// on it rather than running unbounded.
#[test]
fn the_foreground_wait_is_bounded_by_the_reference_ceiling() {
    assert_eq!(timeout_argument(&json!({})), DEFAULT_TIMEOUT_SECONDS);
    assert_eq!(timeout_argument(&json!({"timeout": 5})), 5);
    // The reference reads `args.timeout or default`, so a zero is the default.
    assert_eq!(
        timeout_argument(&json!({"timeout": 0})),
        DEFAULT_TIMEOUT_SECONDS
    );
    assert_eq!(
        timeout_argument(&json!({"timeout": 100_000})),
        MAX_TIMEOUT_SECONDS
    );
    assert_eq!(timeout_argument(&json!({"timeout_seconds": 1.2})), 2);
}

/// Every control key the schema advertises writes bytes, and nothing else does.
#[test]
fn every_advertised_control_key_resolves_to_bytes() {
    let counted = AtomicUsize::new(0);
    for (name, sequence) in CONTROL_KEYS {
        let bytes = stdin_bytes(&json!({"control": [name]})).expect("a known control key");
        assert_eq!(bytes, sequence);
        counted.fetch_add(1, Ordering::Relaxed);
    }
    assert_eq!(counted.load(Ordering::Relaxed), CONTROL_KEYS.len());
    assert!(stdin_bytes(&json!({"control": ["ctrl_shift_q"]})).is_err());
}

//! What the shell families publish, refuse, run and clean up.
//!
//! Every case here drives the tools through [`ToolRegistry::invoke`], which is
//! the path a model call takes: schema validation and default application
//! happen there, so a test that called a handler directly would prove less than
//! it appears to.

use std::sync::atomic::{AtomicUsize, Ordering};

use tempfile::{TempDir, tempdir};

use super::*;
use crate::matching::NameFilter;
use crate::policy::{
    ApprovalDecision, ApprovalFuture, ApprovalRequest, TrustDecision, TrustRootKind,
};
use crate::process::{ClientToolIo, ClientToolRequest};

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
    family: ShellFamily,
    requests: Arc<StdMutex<Vec<String>>>,
}

impl Harness {
    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn shell(&self) -> Arc<SessionShell> {
        self.tools
            .session_shell("session-1", self.family)
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
    harness_on(posix_host(), rollout, decision).await
}

/// A POSIX host, which is what every `bash` case runs against whatever machine
/// the suite is on.
fn posix_host() -> HostShells {
    HostShells {
        platform: Platform::Posix,
        git_bash: None,
        powershell: None,
    }
}

/// A Windows host carrying the named executables, which is how the two Windows
/// families are driven from a POSIX machine.
fn windows_host(git_bash: Option<&str>, powershell: Option<&str>) -> HostShells {
    HostShells {
        platform: Platform::Windows,
        git_bash: git_bash.map(PathBuf::from),
        powershell: powershell.map(PathBuf::from),
    }
}

async fn harness_on(
    host: HostShells,
    rollout: ShellRollout,
    decision: ApprovalDecision,
) -> Harness {
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
    let family = published_family(&host, rollout).map_or(ShellFamily::Bash, |(family, _)| family);
    let tools = ShellTools::with_host(directory.path().join("home"), rollout, host);
    tools
        .register(
            "session-1",
            directory.path(),
            &registry,
            None,
            &ToolGuard::new(policy, approval as Arc<dyn ApprovalAgent>),
        )
        .expect("the shell family registers");
    Harness {
        directory,
        registry,
        tools,
        family,
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
        let mut specs = vec![
            command_spec(ShellFamily::Bash, false),
            command_spec(ShellFamily::Bash, true),
        ];
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

/// A command the analyzer permits outright runs without an approval round
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

/// A command the analyzer does not permit outright is held until the operator
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

/// The legacy variant publishes none of the overrides and honors none of
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
            <= shell_settings().max_output_bytes,
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

/// A refusal names what the analyzer objected to, so the model can propose
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
    let settings = shell_settings();
    assert_eq!(timeout_argument(&json!({}), &settings), 300);
    assert_eq!(timeout_argument(&json!({"timeout": 5}), &settings), 5);
    // The reference reads `args.timeout or default`, so a zero is the default.
    assert_eq!(timeout_argument(&json!({"timeout": 0}), &settings), 300);
    assert_eq!(
        timeout_argument(&json!({"timeout": 100_000}), &settings),
        600
    );
    assert_eq!(
        timeout_argument(&json!({"timeout_seconds": 1.2}), &settings),
        2
    );

    // Both bounds are the operator's to move.
    let resolver = ToolConfigResolver::new();
    resolver.update(
        "[bash]\ndefault_timeout = 12\nmax_timeout_seconds = 30.0\n"
            .parse::<toml::Table>()
            .expect("settings parse"),
    );
    let configured: ShellCommandConfig = resolver.view("bash");
    assert_eq!(timeout_argument(&json!({}), &configured), 12);
    assert_eq!(
        timeout_argument(&json!({"timeout": 100_000}), &configured),
        30
    );
}

/// The declared `bash` configuration, which is what a call resolves when
/// nothing overrides it.
fn shell_settings() -> ShellCommandConfig {
    ToolConfigResolver::new().view("bash")
}

/// US-103: the shell lists follow the interpreter the family drives, not the
/// operating system, which is the branch `register` narrows the resolver to.
/// The Windows host is simulated so the case runs on either one.
#[test]
fn the_shell_lists_follow_the_family_rather_than_the_host() {
    let windows_host = ToolConfigResolver::new().with_posix_shell(false);
    for (family, allowlisted) in [
        (ShellFamily::Bash, 44),
        (ShellFamily::GitBash, 44),
        (ShellFamily::PowerShell, 13),
    ] {
        let settings: ShellCommandConfig = windows_host
            .clone()
            .with_posix_shell(family.uses_posix_shell())
            .view(family.name());
        assert_eq!(
            settings.shared.allowlist.len(),
            allowlisted,
            "`{}` reads the wrong shell branch",
            family.name()
        );
    }
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

// --------------------------------------------------------------------------
// The Windows families
// --------------------------------------------------------------------------

/// A real Git Bash on a POSIX test machine: the executable is a POSIX `bash`,
/// which is what the argument form keys off, so a Windows-family session runs
/// a real process here.
fn git_bash_host() -> HostShells {
    windows_host(Some("/bin/bash"), None)
}

fn family_names(family: ShellFamily) -> Vec<String> {
    let mut names = vec![family.name().to_owned()];
    names
        .extend(["log_file", "output", "sessions", "stdin"].map(|suffix| family.tool_name(suffix)));
    names
}

/// Reference `git_bash_shell_available` is re-read on every publication, so a
/// family whose interpreter is uninstalled while a session runs leaves the
/// surface at the next turn instead of failing at call time.
#[tokio::test]
async fn a_windows_family_leaves_the_surface_when_its_interpreter_goes_away() {
    let directory = tempdir().expect("tempdir");
    let installed = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let probe = Arc::clone(&installed);
    let registry = ToolRegistry::default();
    let tools = ShellTools::with_host_resolver(
        directory.path().join("home"),
        ShellRollout::Managed,
        Arc::new(move || {
            if probe.load(Ordering::Acquire) {
                git_bash_host()
            } else {
                windows_host(None, None)
            }
        }),
    );
    let (approval, _requests) = ScriptedApproval::new(ApprovalDecision::Deny);
    tools
        .register(
            "session-1",
            directory.path(),
            &registry,
            None,
            &ToolGuard::new(
                PermissionStore::default(),
                approval as Arc<dyn ApprovalAgent>,
            ),
        )
        .expect("the Git Bash family registers");
    let published = || {
        registry
            .available(&NameFilter::default(), &NameFilter::default())
            .expect("available")
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>()
    };
    assert_eq!(published(), family_names(ShellFamily::GitBash));

    installed.store(false, Ordering::Release);
    assert!(
        published().is_empty(),
        "an uninstalled interpreter still published: {:?}",
        published()
    );
    assert_eq!(registry.withheld().expect("withheld").len(), 5);
    assert!(matches!(
        registry
            .invoke(
                "git_bash",
                ToolInvocation {
                    call_id: "call-1".to_owned(),
                    arguments: json!({"command": "echo hi"}),
                },
            )
            .await,
        Err(ToolError::Unavailable(name)) if name == "git_bash"
    ));
}

/// The ten Windows names are absent from a POSIX host under either rollout,
/// which is the half of the availability rule this machine can observe
/// directly.
#[tokio::test]
async fn a_posix_host_publishes_no_windows_name() {
    for rollout in [ShellRollout::Legacy, ShellRollout::Managed] {
        let harness = harness(rollout, ApprovalDecision::Deny).await;
        let published = harness.names().join(" ");
        for name in family_names(ShellFamily::GitBash)
            .into_iter()
            .chain(family_names(ShellFamily::PowerShell))
        {
            assert!(
                !published.contains(&name),
                "a POSIX host published `{name}`: {published}"
            );
        }
    }
}

/// Reference `GitBash.is_available`: Windows and a resolvable Git Bash. The
/// five names appear, and the managed variant wins the family name.
#[tokio::test]
async fn a_windows_host_with_git_bash_publishes_the_git_bash_family() {
    let harness = harness_on(
        git_bash_host(),
        ShellRollout::Managed,
        ApprovalDecision::Deny,
    )
    .await;
    assert_eq!(harness.names(), family_names(ShellFamily::GitBash));
    let mut properties = harness.schema("git_bash")["properties"]
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
}

/// Reference `_powershell_treatment_available`: PowerShell publishes only
/// where no Git Bash resolves, so the same host publishes one family or the
/// other and never both.
#[tokio::test]
async fn git_bash_takes_the_windows_host_from_powershell() {
    let both = harness_on(
        windows_host(Some("/bin/bash"), Some("pwsh.exe")),
        ShellRollout::Managed,
        ApprovalDecision::Deny,
    )
    .await;
    assert_eq!(both.names(), family_names(ShellFamily::GitBash));

    let powershell_only = harness_on(
        windows_host(None, Some("pwsh.exe")),
        ShellRollout::Managed,
        ApprovalDecision::Deny,
    )
    .await;
    assert_eq!(
        powershell_only.names(),
        family_names(ShellFamily::PowerShell)
    );
}

/// A Windows host with neither shell installed publishes nothing at all: the
/// reference availability rule fails for both families, and `bash` is withheld
/// on Windows once the managed rollout is on.
#[tokio::test]
async fn a_windows_host_without_either_shell_publishes_nothing() {
    let harness = harness_on(
        windows_host(None, None),
        ShellRollout::Managed,
        ApprovalDecision::Deny,
    )
    .await;
    assert!(harness.names().is_empty(), "{:?}", harness.names());
}

/// Every Windows-family tool carries the reference `managed` rollout, so the
/// legacy rollout publishes the POSIX `bash` name even on Windows and none of
/// the ten.
#[tokio::test]
async fn the_windows_families_stay_absent_under_the_legacy_rollout() {
    let harness = harness_on(
        git_bash_host(),
        ShellRollout::Legacy,
        ApprovalDecision::Deny,
    )
    .await;
    assert_eq!(harness.names(), ["bash"]);
}

/// The legacy Windows variant is registered under the same name and loses to
/// the managed one, which is what reference `selection_priority` does. Its
/// schema is `GitBashArgs`: the four overrides without the two session keys.
#[test]
fn the_legacy_windows_variant_publishes_the_reference_override_set() {
    for family in [ShellFamily::GitBash, ShellFamily::PowerShell] {
        let spec = command_spec(family, false);
        let properties = spec.input_schema["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            properties,
            [
                "command",
                "cwd",
                "env",
                "shell",
                "timeout",
                "timeout_seconds"
            ],
            "{}",
            family.name()
        );
        assert_eq!(spec.selection_priority, LEGACY_SELECTION_PRIORITY);
        assert_eq!(
            command_spec(family, true).selection_priority,
            MANAGED_SELECTION_PRIORITY
        );
    }
}

/// Reference `build_windows_shell_argv` reads the argument form from the
/// executable, not from the family, so an override that points a family at
/// another interpreter still gets that interpreter's flags.
#[test]
fn the_argument_form_follows_the_resolved_executable() {
    assert_eq!(
        windows_shell_arguments(Path::new(r"C:\Program Files\PowerShell\7\pwsh.exe")),
        ["-NoLogo", "-NoProfile", "-Command"]
    );
    assert_eq!(
        windows_shell_arguments(Path::new(r"C:\Windows\System32\powershell.EXE")),
        ["-NoLogo", "-NoProfile", "-Command"]
    );
    assert_eq!(
        windows_shell_arguments(Path::new(r"C:\Program Files\Git\bin\bash.exe")),
        ["-c"]
    );
    assert!(
        windows_shell_arguments(Path::new(r"C:\Windows\System32\cmd.exe")).is_empty(),
        "the reference passes the command straight to anything it does not know"
    );
}

/// The rule reaches the running process, not only the specification: a `shell`
/// override carries the argument form of the executable it names rather than
/// the one the session resolved, which is what reference
/// `build_windows_shell_argv` does with the shell it is handed.
#[cfg(unix)]
#[tokio::test]
async fn a_shell_override_carries_the_argument_form_of_the_executable_it_names() {
    use std::os::unix::fs::PermissionsExt as _;

    let harness = harness_on(
        git_bash_host(),
        ShellRollout::Managed,
        ApprovalDecision::ApproveOnce,
    )
    .await;
    // A stand-in for the PowerShell an operator would point a Windows session
    // at, which reports the argument form it was launched with.
    let executable = harness.root().join("pwsh.exe");
    std::fs::write(&executable, "#!/bin/sh\necho \"$@\"\n").expect("the stand-in shell is written");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
        .expect("the stand-in shell is executable");

    let output = harness
        .call(
            "git_bash",
            json!({"command": "report", "shell": executable.to_string_lossy()}),
        )
        .await
        .expect("the overridden shell runs");
    assert!(
        output
            .model_text
            .contains("-NoLogo -NoProfile -Command report"),
        "{}",
        output.model_text
    );
}

/// Reference `get_windows_bash_path` scans every `PATH` entry rather than
/// stopping at the first hit, so a real Git Bash listed after the WSL launcher
/// still wins, and `WINDOWS_POWERSHELL_DEFAULT_SHELLS` prefers PowerShell 7.
#[test]
fn the_windows_shell_search_skips_the_wsl_launcher_and_keeps_the_reference_order() {
    let root = tempdir().expect("tempdir");
    let system32 = root.path().join("System32");
    let git = root.path().join("Git/bin");
    let seven = root.path().join("pwsh");
    for directory in [&system32, &git, &seven] {
        std::fs::create_dir_all(directory).expect("directory");
    }
    std::fs::write(system32.join("bash.exe"), b"wsl").expect("wsl stub");
    std::fs::write(system32.join("powershell.exe"), b"ps").expect("powershell");
    std::fs::write(git.join("bash.exe"), b"git bash").expect("git bash");
    std::fs::write(seven.join("pwsh.exe"), b"pwsh").expect("pwsh");

    let directories = [system32.clone(), git.clone(), seven.clone()];
    assert_eq!(find_git_bash(&directories), Some(git.join("bash.exe")));
    assert_eq!(find_powershell(&directories), Some(seven.join("pwsh.exe")));
    assert_eq!(
        find_git_bash(std::slice::from_ref(&system32)),
        None,
        "the WSL launcher is not a Git Bash"
    );
    assert_eq!(
        find_powershell(std::slice::from_ref(&system32)),
        Some(system32.join("powershell.exe"))
    );
    assert!(is_wsl_launcher(Path::new(
        r"C:\Users\a\AppData\Local\Microsoft\WindowsApps\bash.exe"
    )));
}

/// A Git Bash session runs a real command, reports through its own tools, and
/// mints an id under its own prefix so the two families never collide in the
/// shared session directory.
#[tokio::test]
async fn a_git_bash_session_runs_and_answers_under_its_own_prefix() {
    let harness = harness_on(
        git_bash_host(),
        ShellRollout::Managed,
        ApprovalDecision::ApproveOnce,
    )
    .await;
    let started = harness
        .call(
            "git_bash",
            json!({"command": "echo from-git-bash", "background": true}),
        )
        .await
        .expect("a Git Bash session starts");
    let session_id = started.typed_result["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    assert!(
        session_id.starts_with("git_bash_"),
        "the session prefix must be the family's: {session_id}"
    );
    assert!(
        !is_family_session_id(ShellFamily::Bash, &session_id),
        "a bash tool must not recognize a git_bash session"
    );

    let polled = harness
        .call(
            "git_bash_output",
            json!({"session_id": session_id, "wait_seconds": 5}),
        )
        .await
        .expect("the session answers");
    assert!(
        polled.typed_result["output"]
            .as_str()
            .expect("output")
            .contains("from-git-bash"),
        "{polled:?}"
    );

    // The log file tool answers for this family's sessions and refuses the
    // other's, which is what the shared session directory needs.
    harness
        .call(
            "git_bash_log_file",
            json!({"action": "read", "session_id": session_id}),
        )
        .await
        .expect("the family reads its own log");
    let refused = harness
        .call(
            "git_bash_log_file",
            json!({"action": "read", "session_id": "bash_1_00"}),
        )
        .await
        .expect_err("another family's session id is refused");
    assert!(refused.to_string().contains("git_bash"), "{refused}");
}

/// The family's environment reaches the process: reference
/// `_get_git_bash_env_overrides` pins the switches that keep a command from
/// waiting on a terminal no operator is watching.
#[tokio::test]
async fn a_windows_family_forces_its_reference_environment() {
    let harness = harness_on(
        git_bash_host(),
        ShellRollout::Managed,
        ApprovalDecision::ApproveOnce,
    )
    .await;
    let output = harness
        .call("git_bash", json!({"command": "echo \"$CI $PAGER $TERM\""}))
        .await
        .expect("the command runs");
    assert!(
        output.model_text.contains("true cat dumb"),
        "{}",
        output.model_text
    );
}

/// A turn cancelled while a Windows-family process group is running leaves no
/// orphan behind.
///
/// The family publishes the managed variant, and a managed session is owned by
/// the Vibe session rather than by the turn that started it, so the guarantee
/// is delivered where the reference delivers it: the session teardown
/// terminates the group whichever turn opened it.
#[tokio::test]
async fn a_cancelled_windows_family_turn_leaves_no_orphaned_process_group() {
    let harness = harness_on(
        git_bash_host(),
        ShellRollout::Managed,
        ApprovalDecision::ApproveOnce,
    )
    .await;
    let marker = harness.root().join("after-the-windows-cancel");
    let invocation = ToolInvocation {
        call_id: "git_bash-1".to_owned(),
        arguments: json!({"command": format!("sleep 4; touch {}", marker.display())}),
    };
    let dropped = tokio::time::timeout(
        Duration::from_millis(400),
        harness.registry.invoke("git_bash", invocation),
    )
    .await;
    assert!(dropped.is_err(), "the call must still be running");

    harness
        .tools
        .close_session("session-1")
        .await
        .expect("the session closes");
    assert!(
        harness.shell().terminals.list().await.is_empty(),
        "the terminal must be released"
    );
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        !marker.exists(),
        "no process group may outlive the session that started it"
    );
}

/// Reference `decode_safe` reads console output by its byte-order mark, which
/// is what keeps PowerShell's UTF-16 from reaching the model as interleaved
/// NULs. An unmarked UTF-16 stream is decoded too; UTF-8 is left alone.
#[test]
fn utf16_console_output_is_decoded_rather_than_interleaved_with_nuls() {
    let marked_le = [b"\xff\xfe".as_slice(), &encode_utf16("hi", true)].concat();
    let marked_be = [b"\xfe\xff".as_slice(), &encode_utf16("hi", false)].concat();
    assert_eq!(decode_output(&marked_le), "hi");
    assert_eq!(decode_output(&marked_be), "hi");
    assert_eq!(
        decode_output(&encode_utf16("Directory: C:\\", true)),
        "Directory: C:\\"
    );
    assert_eq!(
        decode_output(&encode_utf16("Directory: C:\\", false)),
        "Directory: C:\\"
    );
    assert_eq!(decode_output("plain ascii".as_bytes()), "plain ascii");
    assert_eq!(decode_output("héllo ✓".as_bytes()), "héllo ✓");
    assert_eq!(decode_output(b"\xef\xbb\xbfmarked utf-8"), "marked utf-8");
    // Binary output has NULs on both parities, so it is not mistaken for text.
    assert_eq!(decode_output(&[0, 0, 0, 0, 1, 2]).len(), 6);
    assert!(!decode_output(&marked_le).contains('\0'));
}

fn encode_utf16(text: &str, little_endian: bool) -> Vec<u8> {
    text.encode_utf16()
        .flat_map(|unit| {
            if little_endian {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            }
        })
        .collect()
}

/// A UTF-16 stream survives the whole path: a real process writes it, the
/// family captures it, and the model text carries characters rather than NULs.
#[tokio::test]
async fn a_command_writing_utf16_reaches_the_model_as_text() {
    let harness = harness_on(
        git_bash_host(),
        ShellRollout::Managed,
        ApprovalDecision::ApproveOnce,
    )
    .await;
    let output = harness
        .call(
            "git_bash",
            json!({"command": r#"printf '\xff\xfeO\x00K\x00'"#}),
        )
        .await
        .expect("the command runs");
    assert_eq!(
        output.typed_result["output"].as_str().unwrap_or_default(),
        "OK",
        "{output:?}"
    );
}

/// A Git Bash command speaks Git Bash paths while the session root is a
/// Windows one, so the analysis translates `/c/work` onto `C:\work` and sees
/// the file as inside the workspace rather than outside it.
#[test]
fn a_git_bash_path_is_translated_onto_the_windows_workspace_root() {
    let inside = analyze(
        ShellFlavor::GitBash,
        Platform::Windows,
        Path::new(r"C:\work"),
        "cat /c/work/notes.txt",
        &ShellCommandLists::from_config(&shell_settings()),
    );
    assert_eq!(inside.path_operands, ["/c/work/notes.txt"]);
    assert!(
        !inside
            .rationale
            .iter()
            .any(|reason| reason.contains("outside")),
        "a translated path inside the workspace is not an outside-directory ask: {:?}",
        inside.rationale
    );

    // Without the translation the same operand is a root-relative Windows path
    // and lands nowhere near the workspace, which is what the family flavor
    // exists to prevent.
    let untranslated = analyze(
        ShellFlavor::PowerShell,
        Platform::Windows,
        Path::new(r"C:\work"),
        "cat /c/work/notes.txt",
        &ShellCommandLists::from_config(&shell_settings()),
    );
    assert!(
        untranslated
            .rationale
            .iter()
            .any(|reason| reason.contains("outside") || reason.contains("ambiguous")),
        "{:?}",
        untranslated.rationale
    );

    let outside = analyze(
        ShellFlavor::GitBash,
        Platform::Windows,
        Path::new(r"C:\work"),
        "cat /d/secrets/notes.txt",
        &ShellCommandLists::from_config(&shell_settings()),
    );
    assert!(
        outside
            .rationale
            .iter()
            .any(|reason| reason.contains("outside")),
        "another drive is outside the workspace: {:?}",
        outside.rationale
    );
}

// --------------------------------------------------------------------------
// Client terminals
// --------------------------------------------------------------------------

/// A client hosting a terminal, recording the sequence it was driven through.
#[derive(Default)]
struct TerminalClient {
    requests: StdMutex<Vec<ClientToolRequest>>,
    hosts_terminal: bool,
}

impl TerminalClient {
    fn hosting(hosts_terminal: bool) -> Arc<Self> {
        Arc::new(Self {
            requests: StdMutex::new(Vec::new()),
            hosts_terminal,
        })
    }

    fn methods(&self) -> Vec<&'static str> {
        self.requests
            .lock()
            .expect("requests")
            .iter()
            .map(ClientToolRequest::method)
            .collect()
    }
}

impl crate::process::ClientToolPort for TerminalClient {
    fn request<'a>(&'a self, request: ClientToolRequest) -> crate::process::ToolIoFuture<'a> {
        Box::pin(async move {
            self.requests
                .lock()
                .map_err(|_| crate::process::ToolIoError::Request("client lock".to_owned()))?
                .push(request.clone());
            match request {
                ClientToolRequest::TerminalCreate { .. } => {
                    Ok(json!({"terminalId": "editor-terminal-1"}))
                }
                ClientToolRequest::TerminalWait { .. } => Ok(json!({"exitCode": 0})),
                ClientToolRequest::TerminalOutput { .. } => {
                    Ok(json!({"output": "from the editor\n", "truncated": false}))
                }
                _ => Ok(json!({})),
            }
        })
    }

    fn supports(&self, capability: crate::process::ClientToolCapability) -> bool {
        self.hosts_terminal && capability == crate::process::ClientToolCapability::Terminal
    }
}

async fn terminal_client_harness(client: Arc<TerminalClient>) -> Harness {
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
    let (approval, requests) = ScriptedApproval::new(ApprovalDecision::ApproveOnce);
    let registry = ToolRegistry::default();
    let tools = ShellTools::with_host(
        directory.path().join("home"),
        ShellRollout::Legacy,
        posix_host(),
    );
    tools
        .register(
            "session-1",
            directory.path(),
            &registry,
            Some(ClientToolIo::new("session-1", client)),
            &ToolGuard::new(policy, approval as Arc<dyn ApprovalAgent>),
        )
        .expect("the shell family registers");
    Harness {
        directory,
        registry,
        tools,
        family: ShellFamily::Bash,
        requests,
    }
}

/// The command runs in the user's editor rather than in a hidden process, and
/// the terminal it opened is released once the output has been read.
#[tokio::test]
async fn a_client_hosting_a_terminal_runs_the_command_through_it() {
    let client = TerminalClient::hosting(true);
    let harness = terminal_client_harness(client.clone()).await;

    let output = harness
        .call("bash", json!({"command": "echo ok"}))
        .await
        .expect("the client answers the command");
    assert_eq!(output.model_text, "from the editor\n");
    assert_eq!(
        client.methods(),
        [
            "clientTool/terminal/create",
            "clientTool/terminal/wait",
            "clientTool/terminal/output",
            "clientTool/terminal/release",
        ]
    );
    let requests = client.requests.lock().expect("requests");
    let ClientToolRequest::TerminalCreate {
        session_id,
        command,
        cwd,
        output_byte_limit,
        tool_call_id,
        ..
    } = &requests[0]
    else {
        unreachable!("the first request creates the terminal: {requests:?}");
    };
    assert_eq!(session_id, "session-1");
    assert_eq!(command, "echo ok");
    assert_eq!(cwd, &harness.root().to_string_lossy());
    assert!(*output_byte_limit > 0);
    assert_eq!(tool_call_id.as_deref(), Some("bash-1"));
}

/// A client that declared no terminal leaves the command on this host, so a
/// terminal-less editor is not left waiting on a delegation it cannot answer.
#[tokio::test]
async fn a_client_hosting_no_terminal_runs_the_command_on_this_host() {
    let client = TerminalClient::hosting(false);
    let harness = terminal_client_harness(client.clone()).await;

    let output = harness
        .call("bash", json!({"command": "echo local"}))
        .await
        .expect("this host answers the command");
    assert_eq!(output.model_text.trim(), "local");
    assert!(
        client.methods().is_empty(),
        "an undeclared terminal still reached the client: {:?}",
        client.methods()
    );
}

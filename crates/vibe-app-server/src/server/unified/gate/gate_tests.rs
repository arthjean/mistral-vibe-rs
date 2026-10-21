use vibe_core::policy::{PermissionStore, PolicyGuardedTool, ToolGuard};
use vibe_core::tools::{
    OwnedToolHandlerFuture, ToolAvailability, ToolExecutionOutput, ToolHandler, ToolOutputSink,
    ToolPresentationKind, ToolSource, ToolSpec,
};
use vibe_harness::gate::AskFuture;

use super::*;

/// Answers every approval with `answer`, or closes it when there is none,
/// and remembers what each one asked about.
#[derive(Default)]
struct RecordingApprover {
    answer: Option<Value>,
    asked: Mutex<Vec<Vec<Value>>>,
}

impl RecordingApprover {
    fn approving(decision: &str) -> Self {
        Self {
            answer: Some(json!({"type": "approval", "decision": {"type": decision}})),
            ..Self::default()
        }
    }

    fn approving_for_session() -> Self {
        Self::approving("approve_for_session")
    }

    fn asked(&self) -> Vec<Vec<Value>> {
        lock(&self.asked).clone()
    }
}

impl Approver for RecordingApprover {
    fn ask(&self, required_permissions: Vec<Value>, _reason: Option<String>) -> AskFuture<'_> {
        lock(&self.asked).push(required_permissions);
        let answer = match &self.answer {
            Some(output) => CallbackAnswer::Output(output.clone()),
            None => CallbackAnswer::Closed("closed".to_owned()),
        };
        Box::pin(async move { answer })
    }
}

/// A gate over a session that publishes `available` and configures nothing.
fn gate(available: &[&str]) -> UnifiedGate {
    let available: Vec<String> = available.iter().map(|name| (*name).to_owned()).collect();
    UnifiedGate {
        bypass: false,
        edits: false,
        tools: Some(ToolRegistry::new(1024)),
        smart: None,
        provided: super::super::provided::provided(None, &available, None).routes,
        available,
    }
}

/// A gate over a session whose shells answer every command with their
/// context here and record approvals in `store`.
fn shells(store: &PermissionStore, rules: &[(&str, PermissionContext)]) -> UnifiedGate {
    let registry = ToolRegistry::new(1024);
    registry.set_guard(ToolGuard::new(
        store.clone(),
        Arc::new(ProbeAgent::default()),
    ));
    for (name, context) in rules {
        let context = context.clone();
        let handler: Arc<dyn ToolHandler> = Arc::new(
            |_invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
                Box::pin(async { Ok(ToolExecutionOutput::text("ran")) })
            },
        );
        let guarded = PolicyGuardedTool::new(
            *name,
            store.clone(),
            Arc::new(ProbeAgent::default()),
            Arc::new(move |_invocation| Ok(context.clone())),
            handler,
        );
        registry
            .register(
                ToolSpec {
                    name: (*name).to_owned(),
                    description: "a shell".to_owned(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {"command": {"type": "string"}},
                    }),
                    output_schema: None,
                    config: Value::Null,
                    state: Value::Null,
                    availability: ToolAvailability::Available,
                    presentation: ToolPresentationKind::Generic,
                    source: ToolSource::BuiltIn,
                    selection_priority: 0,
                },
                Arc::new(guarded),
            )
            .expect("register");
    }
    let available: Vec<&str> = rules.iter().map(|(name, _)| *name).collect();
    let mut gate = gate(&available);
    gate.tools = Some(registry);
    gate
}

fn action(tool_name: &str) -> ToolAction {
    ToolAction {
        action_id: "action-1".to_owned(),
        turn_id: "turn-1".to_owned(),
        tool_name: tool_name.to_owned(),
        arguments: json!({"command": "ls"}),
    }
}

fn scope(command: &str) -> PermissionRequirement {
    PermissionRequirement {
        session_pattern: format!("{command} *"),
        literal: false,
        ..PermissionRequirement::exact_command(command)
    }
}

fn wire(requirements: &[PermissionRequirement]) -> Vec<Value> {
    requirements
        .iter()
        .map(|requirement| serde_json::to_value(requirement).expect("requirement"))
        .collect()
}

fn ran(source: &str) -> GateVerdict {
    let approval_type = if source == "user" { "ask" } else { "always" };
    GateVerdict::Run {
        approval: approval_record("execute", approval_type, source),
    }
}

#[test]
fn a_shell_builtin_stands_for_every_shell_the_session_publishes() {
    let available = ["powershell".to_owned(), "git_bash".to_owned()];
    assert_eq!(
        vibe_tool_names(gated_sources("file_system.bash").unwrap(), &available),
        ["git_bash", "powershell"]
    );
    assert_eq!(
        vibe_tool_names(gated_sources("process.write").unwrap(), &available),
        ["git_bash", "powershell"]
    );
    assert!(gated_sources("process.output").is_none());
    assert!(gated_sources("ui.ask_user_question").is_none());
}

#[tokio::test]
async fn a_session_with_only_powershell_asks_its_rules_for_a_command() {
    for shell in ["powershell", "git_bash"] {
        let approver = RecordingApprover::default();
        let verdict = gate(&[shell])
            .check(&action("file_system.bash"), &approver)
            .await;
        // The call reaches the shell's configured `ask`, whose callback the
        // approver closed, rather than being refused by policy.
        assert_eq!(approver.asked().len(), 1, "{shell} was not asked");
        assert_eq!(verdict, refused_by_callback(), "{shell}");
    }
}

#[tokio::test]
async fn a_session_without_a_shell_refuses_a_command_by_policy() {
    let approver = RecordingApprover::default();
    let verdict = gate(&["read_file"])
        .check(&action("process.start"), &approver)
        .await;
    assert!(approver.asked().is_empty());
    assert_eq!(verdict, refused_by_policy(None));
}

#[tokio::test]
async fn reading_a_started_process_is_never_gated() {
    let approver = RecordingApprover::default();
    let verdict = gate(&["powershell"])
        .check(&action("process.output"), &approver)
        .await;
    assert!(approver.asked().is_empty());
    assert_eq!(verdict, ran("config"));
}

#[tokio::test]
async fn a_shell_that_refuses_a_command_refuses_it_for_every_shell() {
    let refusal = PermissionContext::settled(PermissionMode::Never).because("refused");
    let asking = PermissionContext::asking(vec![scope("ls")]);
    for rules in [
        [("bash", refusal.clone()), ("powershell", asking.clone())],
        [("bash", asking), ("powershell", refusal)],
    ] {
        let gate = shells(&PermissionStore::default(), &rules);
        let approver = RecordingApprover::approving_for_session();
        let verdict = gate.check(&action("file_system.bash"), &approver).await;
        assert!(approver.asked().is_empty());
        assert_eq!(verdict, refused_by_policy(Some("refused".to_owned())));
    }
}

#[tokio::test]
async fn a_shell_that_clears_a_command_leaves_a_sibling_scope_to_ask() {
    let gate = shells(
        &PermissionStore::default(),
        &[
            ("bash", PermissionContext::settled(PermissionMode::Always)),
            ("powershell", PermissionContext::asking(vec![scope("ls")])),
        ],
    );
    let approver = RecordingApprover::default();
    let verdict = gate.check(&action("file_system.bash"), &approver).await;
    assert_eq!(approver.asked(), [wire(&[scope("ls")])]);
    assert_eq!(verdict, refused_by_callback());
}

#[tokio::test]
async fn a_shell_that_clears_a_command_answers_for_a_sibling_that_reads_nothing() {
    let gate = shells(
        &PermissionStore::default(),
        &[
            ("bash", PermissionContext::settled(PermissionMode::Always)),
            ("powershell", PermissionContext::deferred()),
        ],
    );
    let approver = RecordingApprover::default();
    let verdict = gate.check(&action("file_system.bash"), &approver).await;
    assert!(approver.asked().is_empty());
    assert_eq!(verdict, ran("config"));
}

#[tokio::test]
async fn a_shell_that_reads_nothing_leaves_a_sibling_scope_to_ask() {
    let gate = shells(
        &PermissionStore::default(),
        &[
            ("bash", PermissionContext::deferred()),
            ("powershell", PermissionContext::asking(vec![scope("ls")])),
        ],
    );
    let approver = RecordingApprover::default();
    gate.check(&action("file_system.bash"), &approver).await;
    // The sibling's scope alone, and no fallback that would widen it.
    assert_eq!(approver.asked(), [wire(&[scope("ls")])]);
}

#[tokio::test]
async fn shells_that_both_scope_a_command_are_asked_about_the_union_once() {
    let store = PermissionStore::default();
    let gate = shells(
        &store,
        &[
            ("bash", PermissionContext::asking(vec![scope("ls")])),
            (
                "powershell",
                PermissionContext::asking(vec![scope("Get-ChildItem")]),
            ),
        ],
    );
    let approver = RecordingApprover::approving_for_session();
    let first = gate.check(&action("file_system.bash"), &approver).await;
    assert_eq!(first, ran("user"));
    assert_eq!(
        approver.asked(),
        [wire(&[scope("ls"), scope("Get-ChildItem")])]
    );
    // Every shell recorded the whole approval, so the same call runs unasked.
    let second = gate.check(&action("file_system.bash"), &approver).await;
    assert_eq!(second, ran("config"));
    assert_eq!(approver.asked().len(), 1);
    assert_eq!(store.tool_permission("bash"), None);
}

#[tokio::test]
async fn a_command_no_shell_reads_is_approved_as_written() {
    let store = PermissionStore::default();
    let gate = shells(
        &store,
        &[
            ("bash", PermissionContext::deferred()),
            ("powershell", PermissionContext::deferred()),
        ],
    );
    // A command no shell's default allowlist holds, which would clear it.
    let call = ToolAction {
        arguments: json!({"command": "./deploy.sh"}),
        ..action("file_system.bash")
    };
    let approver = RecordingApprover::approving_for_session();
    let first = gate.check(&call, &approver).await;
    assert_eq!(first, ran("user"));
    assert_eq!(
        approver.asked(),
        [wire(&[PermissionRequirement::exact_command("./deploy.sh")])]
    );
    let second = gate.check(&call, &approver).await;
    assert_eq!(second, ran("config"));
    assert_eq!(approver.asked().len(), 1);
    // The approval named the command, never the shells themselves.
    for shell in ["bash", "powershell"] {
        assert_eq!(store.tool_permission(shell), None, "{shell}");
    }
}

#[tokio::test]
async fn a_write_to_a_process_is_scoped_to_a_minted_id_only() {
    let write = |process_id: &str| ToolAction {
        arguments: json!({"processId": process_id, "input": "y\n"}),
        ..action("process.write")
    };
    let gate = shells(
        &PermissionStore::default(),
        &[("bash", PermissionContext::deferred())],
    );
    let approver = RecordingApprover::default();
    gate.check(&write("process-0123456789ab-cdef"), &approver)
        .await;
    gate.check(&write("*"), &approver).await;
    assert_eq!(
        approver.asked(),
        [
            wire(&[PermissionRequirement::exact_command(
                "process-0123456789ab-cdef"
            )]),
            Vec::new(),
        ]
    );
}

#[tokio::test]
async fn a_permanent_approval_is_kept_for_every_shell_unless_it_names_nothing_lasting() {
    let persisted = Arc::new(Mutex::new(Vec::new()));
    let store = PermissionStore::default().with_allowlist_persistence(Arc::new({
        let persisted = Arc::clone(&persisted);
        move |tool: &str, patterns: &[String]| {
            lock(&persisted).push((tool.to_owned(), patterns.to_vec()));
            Ok(())
        }
    }));
    let gate = shells(
        &store,
        &[
            ("bash", PermissionContext::deferred()),
            ("powershell", PermissionContext::deferred()),
        ],
    );
    let approver = RecordingApprover::approving("approve_permanently");
    // A blank command, a process that dies with the session, and a command.
    for (builtin, arguments) in [
        ("file_system.bash", json!({"command": " "})),
        (
            "process.write",
            json!({"processId": "process-0a-1b", "input": "y\n"}),
        ),
        ("file_system.bash", json!({"command": "./deploy.sh"})),
    ] {
        let call = ToolAction {
            arguments,
            ..action(builtin)
        };
        assert_eq!(gate.check(&call, &approver).await, ran("user"), "{builtin}");
    }
    assert_eq!(approver.asked().len(), 3);
    assert_eq!(
        *lock(&persisted),
        [
            ("bash".to_owned(), vec!["./deploy.sh".to_owned()]),
            ("powershell".to_owned(), vec!["./deploy.sh".to_owned()]),
        ]
    );
}

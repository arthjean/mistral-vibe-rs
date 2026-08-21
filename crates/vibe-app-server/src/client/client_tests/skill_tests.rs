//! Invoked skills and subagents: the synthetic pair a slash turn produces, and
//! the child session a delegation runs in.

use super::*;

/// Answers "done" with no tool calls and records every request's
/// transcript, so a test proves what the model was shown.
struct TranscriptProbeProvider {
    transcripts: std::sync::Mutex<Vec<Vec<ModelMessage>>>,
}

impl CompletionProvider for TranscriptProbeProvider {
    fn complete<'a>(&'a self, input: &'a ProviderInput) -> vibe_core::engine::ProviderFuture<'a> {
        Box::pin(async move {
            if let Ok(mut transcripts) = self.transcripts.lock() {
                transcripts.push(input.messages.clone());
            }
            Ok(AssistantMessage {
                text: "done".to_owned(),
                reasoning: None,
                reasoning_signature: None,
                reasoning_state: Vec::new(),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                refusal: None,
                stop_reason: "stop".to_owned(),
                correlation_id: None,
            })
        })
    }
}

/// The one-skill resolver the driver tests install on the registry, the
/// way `BuiltinTools::register` installs the real one.
struct ProbeSkillResolver;

impl vibe_core::skills::InvokedSkillResolver for ProbeSkillResolver {
    fn resolve(&self, prompt: &str) -> Option<vibe_core::skills::InvokedSkill> {
        let name = prompt
            .trim()
            .strip_prefix('/')?
            .split_whitespace()
            .next()?
            .to_ascii_lowercase();
        (name == "probe").then(|| vibe_core::skills::InvokedSkill {
            name: "probe".to_owned(),
            loaded: ToolExecutionOutput {
                model_text: format!(
                    "name: probe\ncontent: {}\nDo the probing.\n</skill_content>\nskill_dir: None",
                    vibe_core::skills::skill_content_marker("probe")
                ),
                typed_result: json!({"name": "probe"}),
                display: json!({"kind": "skill", "name": "probe"}),
                projected_result: serde_json::Value::Null,
                chunks: Vec::new(),
            },
            already_loaded: ToolExecutionOutput {
                model_text: "name: probe\ncontent: already loaded\nskill_dir: None".to_owned(),
                typed_result: json!({"name": "probe"}),
                display: json!({"kind": "skill", "name": "probe"}),
                projected_result: serde_json::Value::Null,
                chunks: Vec::new(),
            },
        })
    }
}

fn probe_reservation(prompt: &str, tools: ToolRegistry) -> TurnReservation {
    TurnReservation {
        session_id: "session-1".to_owned(),
        turn_id: "turn-1".to_owned(),
        prompt: prompt.to_owned(),
        input: vec![PublicContentBlock::Text {
            text: prompt.to_owned(),
        }],
        prepared_images: None,
        client_user_message_id: None,
        auto_title: None,
        user_display_content: None,
        mention_stats: None,
        working_directory: "/workspace".to_owned(),
        compaction: CompactionSettings::default(),
        intent: SessionIntent::default(),
        tools,
    }
}

/// US-172: a turn whose prompt is `/name` shows the model the synthetic
/// pair right after the user message, resolved through the registry the
/// session registered its tools into.
#[tokio::test]
async fn a_slash_turn_reaches_the_model_as_the_synthetic_pair() {
    let provider = Arc::new(TranscriptProbeProvider {
        transcripts: std::sync::Mutex::new(Vec::new()),
    });
    let driver = LiveTurnDriver::from_provider_for_tests(provider.clone(), "system");
    let tools = ToolRegistry::default();
    tools.set_invoked_skills(Arc::new(ProbeSkillResolver));

    driver
        .run(&probe_reservation("/probe do it", tools))
        .await
        .expect("turn completes");

    let transcripts = provider.transcripts.lock().expect("transcripts");
    let seen = transcripts.first().expect("one request");
    let user = seen
            .iter()
            .position(|message| {
                matches!(message, ModelMessage::User { content, .. } if content == "/probe do it")
            })
            .expect("the prompt stays the user's message");
    assert!(
        matches!(
            &seen[user + 1],
            ModelMessage::Assistant { tool_calls, .. }
                if tool_calls.len() == 1 && tool_calls[0].name == "skill"
        ),
        "the pair follows the user message: {seen:?}"
    );
    assert!(
        matches!(
            &seen[user + 2],
            ModelMessage::Tool { content, is_error: false, .. }
                if content.contains(&vibe_core::skills::skill_content_marker("probe"))
        ),
        "the call is answered before the model speaks: {seen:?}"
    );
}

/// US-173: a context injection carrying the flag appends the pair after
/// its message at the next turn, and one without the flag stays a plain
/// message.
#[tokio::test]
async fn a_flagged_context_injection_appends_the_pair_before_the_turn() {
    for (inject, expected_pairs) in [(true, 1_usize), (false, 0_usize)] {
        let provider = Arc::new(TranscriptProbeProvider {
            transcripts: std::sync::Mutex::new(Vec::new()),
        });
        let driver = LiveTurnDriver::from_provider_for_tests(provider.clone(), "system");
        let tools = ToolRegistry::default();
        tools.set_invoked_skills(Arc::new(ProbeSkillResolver));
        driver
            .inject_context("session-1", "/probe", true, inject)
            .expect("injection queues");

        driver
            .run(&probe_reservation("hello", tools))
            .await
            .expect("turn completes");

        let transcripts = provider.transcripts.lock().expect("transcripts");
        let seen = transcripts.first().expect("one request");
        let pairs = seen
            .iter()
            .filter(|message| matches!(message, ModelMessage::Tool { .. }))
            .count();
        assert_eq!(
            pairs, expected_pairs,
            "injectInvokedSkill={inject}: {seen:?}"
        );
        let injected = seen
                .iter()
                .position(|message| {
                    matches!(message, ModelMessage::User { content, .. } if content == "/probe")
                })
                .expect("the injected message is carried either way");
        if inject {
            assert!(
                matches!(
                    &seen[injected + 1],
                    ModelMessage::Assistant { tool_calls, .. }
                        if tool_calls.len() == 1 && tool_calls[0].name == "skill"
                ),
                "the pair follows the injected message: {seen:?}"
            );
        }
    }
}

/// Captures the tools one turn publishes and, when `task` is among them,
/// calls it with the agent this probe was built for.
///
/// The child turn reaches the same provider, so `child_turns` is what tells a
/// delegation that started from one the policy refused before anything ran.
struct TaskProbeProvider {
    agent: String,
    root_calls: AtomicUsize,
    child_turns: AtomicUsize,
    published: std::sync::Mutex<Vec<String>>,
    delegation_error: std::sync::Mutex<Option<String>>,
}

impl TaskProbeProvider {
    fn published_task(&self) -> bool {
        self.published
            .lock()
            .expect("published")
            .contains(&"task".to_owned())
    }

    fn delegated(&self) -> bool {
        self.child_turns.load(Ordering::Acquire) > 0
    }

    fn refusal(&self) -> Option<String> {
        self.delegation_error.lock().expect("observed").clone()
    }
}

impl CompletionProvider for TaskProbeProvider {
    fn complete<'a>(&'a self, input: &'a ProviderInput) -> vibe_core::engine::ProviderFuture<'a> {
        Box::pin(async move {
            let finished = AssistantMessage {
                text: "done".to_owned(),
                reasoning: None,
                reasoning_signature: None,
                reasoning_state: Vec::new(),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                refusal: None,
                stop_reason: "stop".to_owned(),
                correlation_id: None,
            };
            if input.metadata.contains_key("parent_session_id") {
                self.child_turns.fetch_add(1, Ordering::AcqRel);
                return Ok(finished);
            }
            let call = self.root_calls.fetch_add(1, Ordering::AcqRel);
            if call == 0 {
                if let Ok(mut published) = self.published.lock() {
                    *published = input.tools.iter().map(|tool| tool.name.clone()).collect();
                }
                if input.tools.iter().any(|tool| tool.name == "task") {
                    return Ok(AssistantMessage {
                        text: String::new(),
                        tool_calls: vec![ModelToolCall {
                            id: "delegate-1".to_owned(),
                            name: "task".to_owned(),
                            arguments: json!({"task": "inspect", "agent": self.agent}).to_string(),
                        }],
                        stop_reason: "tool_calls".to_owned(),
                        ..finished
                    });
                }
            }
            if let Ok(mut observed) = self.delegation_error.lock() {
                *observed = input.messages.iter().find_map(|message| match message {
                    ModelMessage::Tool {
                        call_id,
                        content,
                        is_error: true,
                    } if call_id == "delegate-1" => Some(content.clone()),
                    _ => None,
                });
            }
            Ok(finished)
        })
    }
}

/// One turn that publishes `task` and calls it, and what the provider and the
/// approval agent saw while it ran.
struct TaskProbe {
    provider: Arc<TaskProbeProvider>,
    approval: Arc<ScriptedApproval>,
}

/// Runs a turn that delegates to `agent` against a durable parent session, so
/// the delegation can genuinely start and the policy in front of it is the only
/// thing that can stop it.
///
/// `settings` is the operator's `tools` document, and `decision` is what the
/// approval agent answers when the policy asks. `sessions` is [`None`] for a
/// session with no store, which publishes no `task` at all.
async fn run_task_probe(
    sessions: Option<&Path>,
    agent: &str,
    settings: &str,
    decision: ApprovalDecision,
) -> TaskProbe {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let working_directory = sessions.unwrap_or_else(|| temporary.path());
    let provider = Arc::new(TaskProbeProvider {
        agent: agent.to_owned(),
        root_calls: AtomicUsize::new(0),
        child_turns: AtomicUsize::new(0),
        published: std::sync::Mutex::new(Vec::new()),
        delegation_error: std::sync::Mutex::new(None),
    });
    let driver = LiveTurnDriver::from_provider_for_tests(provider.clone(), "system")
        .with_session_root_for_tests(sessions.map(Path::to_path_buf));
    let resume = sessions.map(|root| {
        SessionStore::new(root)
            .create(
                "persisted-root",
                &working_directory.to_string_lossy(),
                None,
                1,
            )
            .expect("durable parent");
        "persisted-root".to_owned()
    });
    let (tools, approval) = guarded_registry(settings, decision);
    driver
        .run(&TurnReservation {
            session_id: "probe".to_owned(),
            turn_id: "probe-turn".to_owned(),
            prompt: "delegate".to_owned(),
            input: vec![PublicContentBlock::Text {
                text: "delegate".to_owned(),
            }],
            prepared_images: None,
            client_user_message_id: None,
            auto_title: None,
            user_display_content: None,
            mention_stats: None,
            working_directory: working_directory.to_string_lossy().into_owned(),
            compaction: CompactionSettings::default(),
            intent: SessionIntent {
                trusted: true,
                resume,
                ..SessionIntent::default()
            },
            tools,
        })
        .await
        .expect("the turn completes");
    TaskProbe { provider, approval }
}

/// Without a session store there is no subagent runner, and the reference
/// rule is that an unavailable tool is withheld rather than published and
/// failed at call time.
#[tokio::test]
async fn task_is_withheld_when_no_subagent_runner_backs_the_session() {
    let probe = run_task_probe(None, "explore", "", ApprovalDecision::ApproveOnce).await;
    assert!(
        !probe.provider.published_task(),
        "task must not be published without a runner"
    );
}

/// US-249: an agent name nothing answers to is refused with the names that do
/// exist, so a model that guessed can correct itself.
#[tokio::test]
async fn an_unknown_subagent_is_refused_with_the_available_names() {
    let temporary = tempfile::tempdir().expect("temporary sessions");
    let probe = run_task_probe(
        Some(temporary.path()),
        "ghost",
        // The name is in neither list, so the policy asks and is answered, and
        // the refusal the model reads is the handler's rather than the guard's.
        "[task]\nallowlist = []\n",
        ApprovalDecision::ApproveOnce,
    )
    .await;
    assert!(
        probe.provider.published_task(),
        "task is published once a runner backs the session"
    );
    let refused = probe
        .provider
        .refusal()
        .expect("the delegation failed back to the model");
    assert!(refused.contains("ghost"), "{refused}");
    assert!(refused.contains("explore"), "{refused}");
    assert!(
        !probe.provider.delegated(),
        "no child runs for a name nothing answers to"
    );
}

/// US-248: `tools.task.allowlist` is unset here, so the declared default
/// `["explore"]` is what resolves, and the built-in subagent is delegated to
/// without the operator being asked. The approval agent would refuse if it were
/// consulted, so a started child is proof that it was not.
#[tokio::test]
async fn the_default_task_allowlist_delegates_to_the_built_in_subagent_without_asking() {
    let temporary = tempfile::tempdir().expect("temporary sessions");
    let probe = run_task_probe(
        Some(temporary.path()),
        "explore",
        "",
        ApprovalDecision::Deny,
    )
    .await;
    assert!(
        !probe.approval.asked_for("task"),
        "an allowlisted agent is granted outright"
    );
    assert!(
        probe.provider.delegated(),
        "the child turn ran: {:?}",
        probe.provider.refusal()
    );
    assert_eq!(probe.provider.refusal(), None);
}

/// US-248: a denylisted agent is refused, and the refusal reaches the model as
/// an error rather than a delegation nobody started.
#[tokio::test]
async fn a_denylisted_subagent_is_refused_before_any_child_starts() {
    let temporary = tempfile::tempdir().expect("temporary sessions");
    let probe = run_task_probe(
        Some(temporary.path()),
        "explore",
        "[task]\ndenylist = [\"explore\"]\n",
        ApprovalDecision::ApproveOnce,
    )
    .await;
    assert!(
        !probe.approval.asked_for("task"),
        "a settled refusal never asks"
    );
    assert!(
        !probe.provider.delegated(),
        "no child starts for a denylisted agent"
    );
    assert!(
        probe.provider.refusal().is_some(),
        "the model reads the refusal"
    );
}

/// US-248: the denylist is consulted first, so a name matching both lists is
/// refused rather than granted.
#[tokio::test]
async fn the_task_denylist_is_consulted_before_the_allowlist() {
    let temporary = tempfile::tempdir().expect("temporary sessions");
    let probe = run_task_probe(
        Some(temporary.path()),
        "explore",
        "[task]\nallowlist = [\"explore\"]\ndenylist = [\"explore\"]\n",
        ApprovalDecision::ApproveOnce,
    )
    .await;
    assert!(
        !probe.provider.delegated(),
        "a name in both lists is refused"
    );
    assert!(probe.provider.refusal().is_some());
}

/// US-248: both lists match by the reference's glob rules, so a wildcard names
/// a family of subagents rather than a subagent literally called `expl*`.
#[tokio::test]
async fn the_task_lists_match_a_subagent_name_as_a_glob() {
    let temporary = tempfile::tempdir().expect("temporary sessions");
    let probe = run_task_probe(
        Some(temporary.path()),
        "explore",
        "[task]\ndenylist = [\"expl*\"]\n",
        ApprovalDecision::ApproveOnce,
    )
    .await;
    assert!(
        !probe.provider.delegated(),
        "the wildcard matched `explore`"
    );
    assert!(probe.provider.refusal().is_some());
}

/// US-248: an agent in neither list falls to the `ask` default, and a declined
/// prompt starts no subagent and hands the model the policy's refusal.
#[tokio::test]
async fn an_unlisted_subagent_asks_the_operator_and_a_decline_starts_no_child() {
    let temporary = tempfile::tempdir().expect("temporary sessions");
    let probe = run_task_probe(
        Some(temporary.path()),
        "explore",
        "[task]\nallowlist = []\n",
        ApprovalDecision::Deny,
    )
    .await;
    assert!(probe.approval.asked_for("task"), "an unlisted agent asks");
    assert!(
        !probe.provider.delegated(),
        "a declined call starts no subagent"
    );
    assert!(
        probe.provider.refusal().is_some(),
        "the model reads the refusal"
    );
}

/// US-248: the same unlisted agent delegates once the operator approves, so the
/// prompt is the whole of what stood between the call and the child.
#[tokio::test]
async fn an_unlisted_subagent_the_operator_approves_still_delegates() {
    let temporary = tempfile::tempdir().expect("temporary sessions");
    let probe = run_task_probe(
        Some(temporary.path()),
        "explore",
        "[task]\nallowlist = []\n",
        ApprovalDecision::ApproveOnce,
    )
    .await;
    assert!(probe.approval.asked_for("task"));
    assert!(
        probe.provider.delegated(),
        "the approved call delegated: {:?}",
        probe.provider.refusal()
    );
}

#[tokio::test]
async fn live_task_tool_runs_a_durable_child_session_through_the_provider() {
    let temporary = tempfile::tempdir().expect("temporary sessions");
    let provider = Arc::new(SubagentSelectingProvider {
        root_calls: AtomicUsize::new(0),
        child_calls: AtomicUsize::new(0),
        saw_task_definition: AtomicBool::new(false),
        published_task_parameters: std::sync::Mutex::new(None),
        child_hid_task_definition: AtomicBool::new(false),
        child_inherited_restrictions: AtomicBool::new(false),
    });
    let driver = LiveTurnDriver::from_provider_for_tests(provider.clone(), "system")
        .with_session_root_for_tests(Some(temporary.path().to_path_buf()));
    let store = SessionStore::new(temporary.path());
    store
        .create(
            "persisted-root",
            &temporary.path().to_string_lossy(),
            None,
            1,
        )
        .expect("durable parent");
    // The child inherits the parent's surface, so the guard the session's
    // builtin registration installs is what `task` is published behind here
    // too; `explore` is allowlisted by default, so nothing is asked.
    let (tools, _approval) = guarded_registry("", ApprovalDecision::Deny);
    for (name, presentation) in [
        ("read", ToolPresentationKind::Read),
        ("edit", ToolPresentationKind::Diff),
        ("shell", ToolPresentationKind::Shell),
    ] {
        tools
            .register(
                ToolSpec {
                    name: name.to_owned(),
                    description: format!("{name} test tool"),
                    input_schema: json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }),
                    output_schema: None,
                    config: Value::Null,
                    state: Value::Null,
                    availability: ToolAvailability::Available,
                    presentation,
                    source: ToolSource::BuiltIn,
                    selection_priority: 10,
                },
                Arc::new(
                    |_invocation: &vibe_core::tools::ToolInvocation,
                     _output: vibe_core::tools::ToolOutputSink|
                     -> vibe_core::tools::OwnedToolHandlerFuture {
                        Box::pin(async { Ok(ToolExecutionOutput::text("unexpected")) })
                    },
                ),
            )
            .expect("test tool");
    }
    let outcome = driver
        .run(&TurnReservation {
            session_id: "runtime-alias".to_owned(),
            turn_id: "root-turn".to_owned(),
            prompt: "delegate".to_owned(),
            input: vec![PublicContentBlock::Text {
                text: "delegate".to_owned(),
            }],
            prepared_images: None,
            client_user_message_id: None,
            auto_title: None,
            user_display_content: None,
            mention_stats: None,
            working_directory: temporary.path().to_string_lossy().into_owned(),
            compaction: CompactionSettings::default(),
            intent: SessionIntent {
                trusted: true,
                enabled_tools: vec!["task".to_owned(), "read".to_owned(), "edit".to_owned()],
                disabled_tools: vec!["shell".to_owned()],
                resume: Some("persisted-root".to_owned()),
                ..SessionIntent::default()
            },
            tools,
        })
        .await
        .expect("root and child complete");
    assert_eq!(outcome.stop_reason, PublicTurnStopReason::Complete);
    assert!(provider.saw_task_definition.load(Ordering::Acquire));
    assert_eq!(
        provider
            .published_task_parameters
            .lock()
            .expect("published schema")
            .clone()
            .expect("the parent turn published `task`"),
        json!({
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "The task for the subagent to perform"},
                "agent": {
                    "type": "string",
                    "description": "Which specialized subagent runs the task",
                    "default": "explore",
                },
            },
            "required": ["task"],
            "additionalProperties": false,
        })
    );
    assert!(provider.child_hid_task_definition.load(Ordering::Acquire));
    assert!(
        provider
            .child_inherited_restrictions
            .load(Ordering::Acquire)
    );
    let page = store.list(None, 0, 10).expect("sessions list");
    assert_eq!(page.sessions.len(), 2);
    let child = page
        .sessions
        .iter()
        .find(|session| session.parent_session_id.as_deref() == Some("persisted-root"))
        .expect("child session");
    assert_eq!(
        store
            .load(&child.id)
            .expect("child hydrates")
            .messages
            .iter()
            .filter_map(|message| match message {
                ModelMessage::Assistant { content, .. } if !content.is_empty() => {
                    Some(content.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>(),
        ["child answer"]
    );
    assert_eq!(
        store
            .continue_session(
                &temporary.path().to_string_lossy(),
                "system",
                BTreeMap::new(),
            )
            .expect("root pointer remains authoritative")
            .metadata
            .id,
        "persisted-root"
    );
}

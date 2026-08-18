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
                chunks: Vec::new(),
            },
            already_loaded: ToolExecutionOutput {
                model_text: "name: probe\ncontent: already loaded\nskill_dir: None".to_owned(),
                typed_result: json!({"name": "probe"}),
                display: json!({"kind": "skill", "name": "probe"}),
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
/// calls it with an agent that does not exist.
struct TaskProbeProvider {
    calls: AtomicUsize,
    published: std::sync::Mutex<Vec<String>>,
    delegation_error: std::sync::Mutex<Option<String>>,
}

impl CompletionProvider for TaskProbeProvider {
    fn complete<'a>(&'a self, input: &'a ProviderInput) -> vibe_core::engine::ProviderFuture<'a> {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, Ordering::AcqRel);
            if call == 0 {
                if let Ok(mut published) = self.published.lock() {
                    *published = input.tools.iter().map(|tool| tool.name.clone()).collect();
                }
                if input.tools.iter().any(|tool| tool.name == "task") {
                    return Ok(AssistantMessage {
                        text: String::new(),
                        reasoning: None,
                        reasoning_signature: None,
                        reasoning_state: Vec::new(),
                        tool_calls: vec![ModelToolCall {
                            id: "delegate-1".to_owned(),
                            name: "task".to_owned(),
                            arguments: r#"{"task":"inspect","agent":"ghost"}"#.to_owned(),
                        }],
                        usage: Usage::default(),
                        refusal: None,
                        stop_reason: "tool_calls".to_owned(),
                        correlation_id: None,
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

async fn run_task_probe(session_root: Option<PathBuf>) -> Arc<TaskProbeProvider> {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let provider = Arc::new(TaskProbeProvider {
        calls: AtomicUsize::new(0),
        published: std::sync::Mutex::new(Vec::new()),
        delegation_error: std::sync::Mutex::new(None),
    });
    let driver = LiveTurnDriver::from_provider_for_tests(provider.clone(), "system")
        .with_session_root_for_tests(session_root);
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
            working_directory: temporary.path().to_string_lossy().into_owned(),
            compaction: CompactionSettings::default(),
            intent: SessionIntent {
                trusted: true,
                ..SessionIntent::default()
            },
            tools: ToolRegistry::default(),
        })
        .await
        .expect("the turn completes");
    provider
}

/// Without a session store there is no subagent runner, and the reference
/// rule is that an unavailable tool is withheld rather than published and
/// failed at call time.
#[tokio::test]
async fn task_is_withheld_when_no_subagent_runner_backs_the_session() {
    let provider = run_task_probe(None).await;
    assert!(
        !provider
            .published
            .lock()
            .expect("published")
            .contains(&"task".to_owned()),
        "task must not be published without a runner"
    );
}

/// An agent name nothing answers to is refused with the names that do
/// exist, so a model that guessed can correct itself.
#[tokio::test]
async fn an_unknown_subagent_is_refused_with_the_available_names() {
    let temporary = tempfile::tempdir().expect("temporary sessions");
    let provider = run_task_probe(Some(temporary.path().to_path_buf())).await;
    assert!(
        provider
            .published
            .lock()
            .expect("published")
            .contains(&"task".to_owned()),
        "task is published once a runner backs the session"
    );
    let refused = provider
        .delegation_error
        .lock()
        .expect("observed")
        .clone()
        .expect("the delegation failed back to the model");
    assert!(refused.contains("ghost"), "{refused}");
    assert!(refused.contains("explore"), "{refused}");
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
    let tools = ToolRegistry::default();
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

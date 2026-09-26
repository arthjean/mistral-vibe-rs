//! The live driver end to end: durable resume, the context warning, compaction
//! and the tool registry a session executes through.

use super::*;

#[test]
fn all_programmatic_intent_crosses_the_json_boundary_unchanged() {
    let mut client = InProcessClient::connect().expect("client connects");
    let options = options();
    let session_id = client.start_session(&options).expect("session starts");
    let view = client.session(&session_id).expect("session reads");
    assert_eq!(view.working_directory, options.working_directory);
    assert_eq!(view.intent.add_directories, options.add_directories);
    assert_eq!(view.intent.agent, options.agent);
    assert_eq!(view.intent.max_turns, options.max_turns);
    assert_eq!(view.intent.max_tokens, options.max_tokens);
    assert!(view.intent.trusted);
    assert!(view.intent.auto_approve);
}

#[tokio::test]
async fn live_driver_hydrates_and_extends_a_durable_resume() {
    let temporary = tempfile::tempdir().expect("temporary session root");
    let store = SessionStore::new(temporary.path());
    let mut metadata = store
        .create("session-resume", "/workspace", None, 1)
        .expect("session creates");
    store
        .append_message(
            &mut metadata,
            &ModelMessage::System {
                content: "old system".to_owned(),
            },
            2,
        )
        .expect("old system persists");
    store
        .append_message(
            &mut metadata,
            &ModelMessage::user("prior question".to_owned()),
            3,
        )
        .expect("prior message persists");
    metadata.statistics.insert(
        "session_prompt_tokens".to_owned(),
        serde_json::Value::from(10),
    );
    metadata.statistics.insert(
        "session_completion_tokens".to_owned(),
        serde_json::Value::from(4),
    );
    metadata
        .statistics
        .insert("context_tokens".to_owned(), serde_json::Value::from(8));
    metadata
        .statistics
        .insert("steps".to_owned(), serde_json::Value::from(2));
    store
        .update_metadata(&metadata)
        .expect("baseline stats persist");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let driver = LiveTurnDriver::from_provider_for_tests(
        Arc::new(RecordingProvider {
            seen: Arc::clone(&seen),
        }),
        "current system",
    )
    .with_session_root_for_tests(Some(temporary.path().to_path_buf()));
    let outcome = driver
        .run(&TurnReservation {
            session_id: "session-resume".to_owned(),
            turn_id: "turn-1".to_owned(),
            prompt: "new question".to_owned(),
            input: vec![PublicContentBlock::Text {
                text: "new question".to_owned(),
            }],
            prepared_images: None,
            injected: false,
            client_user_message_id: None,
            auto_title: None,
            user_display_content: None,
            mention_stats: None,
            working_directory: "/workspace".to_owned(),
            compaction: CompactionSettings::default(),
            intent: SessionIntent {
                resume: Some("session-resume".to_owned()),
                ..SessionIntent::default()
            },
            tools: guarded_registry("", ApprovalDecision::ApproveOnce).0,
        })
        .await
        .expect("resumed turn completes");
    assert_eq!(outcome.session_id, "session-resume");
    assert_eq!(outcome.usage.input_tokens, 13);
    assert_eq!(outcome.usage.output_tokens, 6);
    assert_eq!(outcome.context_tokens, 5);
    // The operator's message is a step, and so is the model turn answering it.
    assert_eq!(outcome.steps, 4);
    let seen = seen.lock().expect("seen messages");
    assert!(matches!(
        seen.first(),
        Some(ModelMessage::System { content }) if content == "current system"
    ));
    assert!(seen.iter().any(|message| matches!(
        message,
        ModelMessage::User { content, .. } if content == "prior question"
    )));
    drop(seen);
    let persisted = store.load("session-resume").expect("extended transcript");
    assert!(persisted.messages.iter().any(|message| matches!(
        message,
        ModelMessage::Assistant { content, .. } if content == "resumed answer"
    )));
    assert_eq!(persisted.metadata.statistics["session_prompt_tokens"], 13);
    assert_eq!(
        persisted.metadata.statistics["session_completion_tokens"],
        6
    );
    assert_eq!(persisted.metadata.statistics["context_tokens"], 5);
    assert_eq!(persisted.metadata.statistics["steps"], 4);
}

/// US-157: the warning reaches the model itself, once, on the turn that
/// crosses half the window, and never again while the session lives.
///
/// The proof starts at the driver rather than at the pipeline, because the
/// latch is what the wiring has to get right: the engine is rebuilt for
/// every turn, so a policy owned by the engine would warn on each of them.
#[tokio::test]
async fn the_context_warning_reaches_the_model_once_per_session() {
    struct TranscriptRecordingProvider {
        turns: Arc<Mutex<Vec<Vec<ModelMessage>>>>,
    }

    impl CompletionProvider for TranscriptRecordingProvider {
        fn complete<'a>(
            &'a self,
            input: &'a ProviderInput,
        ) -> vibe_core::engine::ProviderFuture<'a> {
            Box::pin(async move {
                self.turns
                    .lock()
                    .map_err(|_| {
                        vibe_core::provider::ProviderError::MalformedStream(
                            "test lock poisoned".to_owned(),
                        )
                    })?
                    .push(input.messages.clone());
                Ok(AssistantMessage {
                    text: "answered".to_owned(),
                    reasoning: None,
                    reasoning_payloads: Vec::new(),
                    tool_calls: Vec::new(),
                    // The context stays above half the window and below it,
                    // so the second turn is a real chance to warn again.
                    usage: Usage {
                        input_tokens: 150,
                        output_tokens: 10,
                    },
                    refusal: None,
                    stop_reason: "stop".to_owned(),
                    correlation_id: None,
                })
            })
        }
    }

    async fn run_two_turns(context_warnings: bool) -> Vec<Vec<ModelMessage>> {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let store = SessionStore::new(temporary.path());
        let mut metadata = store
            .create("warned", "/workspace", None, 1)
            .expect("session creates");
        metadata
            .statistics
            .insert("context_tokens".to_owned(), serde_json::Value::from(100));
        store
            .update_metadata(&metadata)
            .expect("baseline stats persist");
        let turns = Arc::new(Mutex::new(Vec::new()));
        let driver = LiveTurnDriver::from_provider_for_tests(
            Arc::new(TranscriptRecordingProvider {
                turns: Arc::clone(&turns),
            }),
            "system",
        )
        .with_session_root_for_tests(Some(temporary.path().to_path_buf()));
        let compaction = CompactionSettings {
            auto_compact_threshold: 200,
            context_warnings,
            ..CompactionSettings::default()
        };
        for turn in ["turn-1", "turn-2"] {
            driver
                .run(&TurnReservation {
                    session_id: "warned".to_owned(),
                    turn_id: turn.to_owned(),
                    prompt: "question".to_owned(),
                    input: vec![PublicContentBlock::Text {
                        text: "question".to_owned(),
                    }],
                    prepared_images: None,
                    injected: false,
                    client_user_message_id: None,
                    auto_title: None,
                    user_display_content: None,
                    mention_stats: None,
                    working_directory: "/workspace".to_owned(),
                    compaction: compaction.clone(),
                    intent: SessionIntent::default(),
                    tools: guarded_registry("", ApprovalDecision::ApproveOnce).0,
                })
                .await
                .expect("the turn completes");
        }
        let recorded = turns.lock().expect("recorded turns");
        recorded.clone()
    }

    // The second request replays the first one's transcript, warning
    // included, so the falsifier is the count rather than the presence.
    let warnings = |messages: &[ModelMessage]| {
        messages
            .iter()
            .filter(|message| {
                matches!(message, ModelMessage::User { content, injected, .. }
                        if *injected && content.contains("<vibe_warning>"))
            })
            .count()
    };

    let enabled = run_two_turns(true).await;
    assert_eq!(enabled.len(), 2, "both turns reached the provider");
    assert_eq!(
        warnings(&enabled[0]),
        1,
        "the first turn past half the window carries the warning: {:?}",
        enabled[0]
    );
    assert_eq!(
        warnings(&enabled[1]),
        1,
        "the second turn replays the first warning and adds none: {:?}",
        enabled[1]
    );

    let disabled = run_two_turns(false).await;
    assert!(
        disabled.iter().all(|messages| warnings(messages) == 0),
        "context_warnings off registers no policy, so nothing is injected"
    );
}

/// US-148: `compaction_model` reaches the summarization request as the
/// model it overrides the provider's own with, which is what
/// `get_compaction_model` selects upstream.
#[tokio::test]
async fn the_configured_compaction_model_overrides_the_summarization_request() {
    struct ModelRecordingProvider {
        models: Mutex<Vec<Option<String>>>,
    }

    impl CompletionProvider for ModelRecordingProvider {
        fn complete<'a>(
            &'a self,
            input: &'a ProviderInput,
        ) -> vibe_core::engine::ProviderFuture<'a> {
            Box::pin(async move {
                self.models
                    .lock()
                    .map_err(|_| {
                        vibe_core::provider::ProviderError::MalformedStream(
                            "test lock poisoned".to_owned(),
                        )
                    })?
                    .push(input.model_override.clone());
                Ok(AssistantMessage {
                    text: "<summary>a summary</summary>".to_owned(),
                    reasoning: None,
                    reasoning_payloads: Vec::new(),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    refusal: None,
                    stop_reason: "stop".to_owned(),
                    correlation_id: None,
                })
            })
        }
    }

    let provider = Arc::new(ModelRecordingProvider {
        models: Mutex::new(Vec::new()),
    });
    let compactor = ProviderSessionCompactor::new(Arc::clone(&provider) as Arc<_>);
    let messages = [ModelMessage::System {
        content: "system".to_owned(),
    }];

    compactor
        .compact("session-1", &messages)
        .await
        .expect("the default compaction summarizes");
    compactor
        .with_plan(CompactionPlan {
            model: Some("devstral-small-latest".to_owned()),
            ..CompactionPlan::default()
        })
        .compact("session-1", &messages)
        .await
        .expect("the configured compaction summarizes");

    assert_eq!(
        provider.models.lock().expect("model log").clone(),
        vec![None, Some("devstral-small-latest".to_owned())],
        "an unset key leaves the provider's model, and a set one overrides it"
    );
}

/// US-152, US-153: an answer with no summary element is classified as the
/// empty-summary failure, the fallback gets its one attempt, and outside
/// strict mode the conversation still compacts under the placeholder while
/// the classified reason is reported. Strict mode fails instead.
#[tokio::test]
async fn an_empty_summary_is_reported_as_the_classified_failure() {
    struct SilentProvider;

    impl CompletionProvider for SilentProvider {
        fn complete<'a>(
            &'a self,
            _input: &'a ProviderInput,
        ) -> vibe_core::engine::ProviderFuture<'a> {
            Box::pin(async {
                Ok(AssistantMessage {
                    text: "   ".to_owned(),
                    reasoning: None,
                    reasoning_payloads: Vec::new(),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    refusal: None,
                    stop_reason: "stop".to_owned(),
                    correlation_id: None,
                })
            })
        }
    }

    let messages = [ModelMessage::System {
        content: "system".to_owned(),
    }];
    let compactor = ProviderSessionCompactor::new(Arc::new(SilentProvider) as Arc<_>);
    let degraded = compactor
        .compact("session-1", &messages)
        .await
        .expect("outside strict mode the conversation still compacts");
    assert_eq!(
        degraded.failure,
        Some(CompactionFailureReason::EmptySummary),
        "the placeholder still reports what it degraded from"
    );
    assert_eq!(degraded.summary, PLACEHOLDER_SUMMARY);

    let failure = compactor
        .with_plan(CompactionPlan {
            strict: true,
            ..CompactionPlan::default()
        })
        .compact("session-1", &messages)
        .await
        .expect_err("strict mode fails the compaction");
    assert_eq!(failure.reason, Some(CompactionFailureReason::EmptySummary));
}

#[tokio::test]
async fn manual_compaction_uses_provider_summary_and_appends_a_boundary() {
    /// Answers with the summary element the summarizer reads, which is what
    /// a model that followed the compaction request returns.
    struct SummarizingProvider {
        seen: Arc<Mutex<Vec<ModelMessage>>>,
    }

    impl CompletionProvider for SummarizingProvider {
        fn complete<'a>(
            &'a self,
            input: &'a ProviderInput,
        ) -> vibe_core::engine::ProviderFuture<'a> {
            Box::pin(async move {
                *self.seen.lock().map_err(|_| {
                    vibe_core::provider::ProviderError::MalformedStream(
                        "test lock poisoned".to_owned(),
                    )
                })? = input.messages.clone();
                Ok(AssistantMessage {
                    text: "<summary>resumed answer</summary>".to_owned(),
                    reasoning: None,
                    reasoning_payloads: Vec::new(),
                    tool_calls: Vec::new(),
                    usage: Usage {
                        input_tokens: 3,
                        output_tokens: 2,
                    },
                    refusal: None,
                    stop_reason: "stop".to_owned(),
                    correlation_id: None,
                })
            })
        }
    }

    let temporary = tempfile::tempdir().expect("temporary session root");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let driver = LiveTurnDriver::from_provider_for_tests(
        Arc::new(SummarizingProvider {
            seen: Arc::clone(&seen),
        }),
        "current system",
    )
    .with_session_root_for_tests(Some(temporary.path().to_path_buf()));
    let mut service = HeadlessService::new(driver).expect("service");
    let mut compact_options = options();
    compact_options.working_directory = temporary.path().to_string_lossy().into_owned();
    compact_options.session_id = Some("manual-compact".to_owned());
    compact_options.add_directories.clear();
    compact_options.tool_filters.clear();
    compact_options.enabled_tools.clear();
    compact_options.disabled_tools.clear();
    compact_options.agent = None;
    let session_id = service.start_session(&compact_options).expect("session");
    service
        .prompt(&session_id, "retain this decision")
        .await
        .expect("turn");

    let result = service
        .compact(&session_id, "Keep exact file paths")
        .await
        .expect("compaction");
    assert_eq!(result["summary"], "resumed answer");
    // Reference `_handler._compact`: the session keeps its identifier, the
    // conversation stays on disk, and the envelope is written after it.
    assert_eq!(result["state"]["session"]["id"], session_id.as_str());
    let compacted = SessionStore::new(temporary.path())
        .load(&session_id)
        .expect("durable compacted session");
    assert!(compacted.messages.iter().any(|message| matches!(
        message,
        ModelMessage::User { content, injected: false, .. } if content == "retain this decision"
    )));
    assert!(matches!(
        compacted.messages.last(),
        Some(ModelMessage::User { content, injected: true, .. })
            if content.contains("<compaction_summary>")
                && content.contains("resumed answer")
                && content.contains("retain this decision")
    ));
    let history = result["state"]["history"]
        .as_array()
        .expect("public history");
    let checkpoint = history.last().expect("a closing checkpoint");
    assert_eq!(checkpoint["kind"], "compaction");
    assert_eq!(
        checkpoint["details"]["summaryLength"],
        "resumed answer".chars().count()
    );
    assert!(seen.lock().expect("provider input").iter().any(|message| {
        matches!(
            message,
            ModelMessage::User { content, .. } if content.contains("Keep exact file paths")
        )
    }));
}

/// A compaction keeps the session it ran in, as upstream does since the
/// envelope is appended: every compaction adds one, and the next one
/// summarizes only from the latest on.
#[tokio::test]
async fn compacting_twice_keeps_the_identity_and_appends_two_boundaries() {
    struct SummarizingProvider {
        seen: Arc<Mutex<Vec<Vec<ModelMessage>>>>,
    }

    impl CompletionProvider for SummarizingProvider {
        fn complete<'a>(
            &'a self,
            input: &'a ProviderInput,
        ) -> vibe_core::engine::ProviderFuture<'a> {
            Box::pin(async move {
                self.seen
                    .lock()
                    .map_err(|_| {
                        vibe_core::provider::ProviderError::MalformedStream(
                            "test lock poisoned".to_owned(),
                        )
                    })?
                    .push(input.messages.clone());
                Ok(AssistantMessage {
                    text: "<summary>the state so far</summary>".to_owned(),
                    reasoning: None,
                    reasoning_payloads: Vec::new(),
                    tool_calls: Vec::new(),
                    usage: Usage {
                        input_tokens: 3,
                        output_tokens: 2,
                    },
                    refusal: None,
                    stop_reason: "stop".to_owned(),
                    correlation_id: None,
                })
            })
        }
    }

    let temporary = tempfile::tempdir().expect("temporary session root");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let driver = LiveTurnDriver::from_provider_for_tests(
        Arc::new(SummarizingProvider {
            seen: Arc::clone(&seen),
        }),
        "sys",
    )
    .with_session_root_for_tests(Some(temporary.path().to_path_buf()));
    let mut service = HeadlessService::new(driver).expect("service");
    let mut compact_options = options();
    compact_options.working_directory = temporary.path().to_string_lossy().into_owned();
    compact_options.session_id = Some("11111111-2222-3333-4444-abcdefabcdef".to_owned());
    compact_options.add_directories.clear();
    compact_options.tool_filters.clear();
    compact_options.enabled_tools.clear();
    compact_options.disabled_tools.clear();
    compact_options.agent = None;
    let session_id = service.start_session(&compact_options).expect("session");

    for prompt in ["the first decision", "the second decision"] {
        service.prompt(&session_id, prompt).await.expect("turn");
        let result = service.compact(&session_id, "").await.expect("compaction");
        assert_eq!(result["state"]["session"]["id"], session_id.as_str());
    }

    let stored = SessionStore::new(temporary.path())
        .load(&session_id)
        .expect("the session is still the one written");
    let envelopes = stored
        .messages
        .iter()
        .filter(|message| vibe_core::compaction::context::is_compaction_context_message(message))
        .count();
    assert_eq!(envelopes, 2);
    // The second summary reads from the first envelope on, so the turn the
    // first one already summarized is not sent again.
    let second_summary = seen
        .lock()
        .expect("provider input")
        .last()
        .cloned()
        .expect("the second compaction called the provider");
    assert!(!second_summary.iter().any(|message| matches!(
        message,
        ModelMessage::User { content, injected: false, .. } if content == "the first decision"
    )));
}

#[tokio::test]
async fn live_driver_exposes_and_executes_the_session_tool_registry() {
    let tools = ToolRegistry::default();
    tools
        .register(
            ToolSpec {
                name: "mcp_fixture_echo".to_owned(),
                description: "Echo through MCP".to_owned(),
                input_schema: ObjectSchema::new()
                    .required("message", Property::string())
                    .build(),
                output_schema: None,
                config: Value::Null,
                state: Value::Null,
                availability: ToolAvailability::Available,
                presentation: ToolPresentationKind::Mcp,
                source: ToolSource::Mcp,
                selection_priority: 50,
            },
            Arc::new(
                |_invocation: &vibe_core::tools::ToolInvocation,
                 _output: vibe_core::tools::ToolOutputSink|
                 -> vibe_core::tools::OwnedToolHandlerFuture {
                    Box::pin(async {
                        Ok(ToolExecutionOutput {
                            skip: None,
                            turn_failure: None,
                            approval: None,
                            failure: None,
                            typed_result: json!({"echo": "rust"}),
                            model_text: "hello rust".to_owned(),
                            display: Value::Null,
                            projected_result: serde_json::Value::Null,
                            chunks: Vec::new(),
                        })
                    })
                },
            ),
        )
        .expect("register test MCP tool");
    let saw_definition = Arc::new(AtomicBool::new(false));
    let driver = LiveTurnDriver::from_provider_for_tests(
        Arc::new(ToolSelectingProvider {
            calls: AtomicUsize::new(0),
            saw_definition: Arc::clone(&saw_definition),
        }),
        "system",
    );
    let outcome = driver
        .run(&TurnReservation {
            session_id: "session-1".to_owned(),
            turn_id: "turn-1".to_owned(),
            prompt: "use MCP".to_owned(),
            input: vec![PublicContentBlock::Text {
                text: "use MCP".to_owned(),
            }],
            prepared_images: None,
            injected: false,
            client_user_message_id: None,
            auto_title: None,
            user_display_content: None,
            mention_stats: None,
            working_directory: "/workspace".to_owned(),
            compaction: CompactionSettings::default(),
            intent: SessionIntent::default(),
            tools,
        })
        .await
        .expect("live turn completes");
    assert_eq!(outcome.stop_reason, PublicTurnStopReason::Complete);
    assert!(saw_definition.load(Ordering::Acquire));
}

/// A registry carrying the names a filtering test needs to tell apart.
fn filtering_registry() -> ToolRegistry {
    let tools = ToolRegistry::default();
    for name in [
        "read_file",
        "serena_find",
        "serena_replace",
        "web_fetch",
        "web_search",
    ] {
        tools
            .register(
                ToolSpec {
                    name: name.to_owned(),
                    description: "fixture".to_owned(),
                    input_schema: ObjectSchema::new().build(),
                    output_schema: None,
                    config: Value::Null,
                    state: Value::Null,
                    availability: ToolAvailability::Available,
                    presentation: ToolPresentationKind::Generic,
                    source: ToolSource::BuiltIn,
                    selection_priority: 0,
                },
                Arc::new(
                    |_invocation: &vibe_core::tools::ToolInvocation,
                     _output: vibe_core::tools::ToolOutputSink|
                     -> vibe_core::tools::OwnedToolHandlerFuture {
                        Box::pin(async { Ok(ToolExecutionOutput::text("fixture")) })
                    },
                ),
            )
            .expect("fixture tool registers");
    }
    tools
}

/// The names a session publishes to the model under one pair of filters,
/// taken from the definitions the turn actually sends.
fn published_under(enabled: &[&str], disabled: &[&str]) -> Vec<String> {
    let executor = SessionToolExecutor::new(
        filtering_registry(),
        &SessionIntent {
            enabled_tools: enabled.iter().map(|entry| (*entry).to_owned()).collect(),
            disabled_tools: disabled.iter().map(|entry| (*entry).to_owned()).collect(),
            ..SessionIntent::default()
        },
    );
    executor
        .definitions()
        .expect("definitions")
        .into_iter()
        .map(|definition| definition.name)
        .collect()
}

/// Reference `available_tools` matches both filter lists with `name_matches`
/// rather than by exact name, so a shared configuration file selects the
/// same surface in both clients.
#[test]
fn configured_tool_filters_match_by_glob_regular_expression_and_case() {
    assert_eq!(
        published_under(&[], &["serena_*"]),
        ["read_file", "web_fetch", "web_search"]
    );
    assert_eq!(
        published_under(&[], &["re:web_.*"]),
        ["read_file", "serena_find", "serena_replace"]
    );
    assert_eq!(
        published_under(&[], &["SERENA_FIND"]),
        ["read_file", "serena_replace", "web_fetch", "web_search"]
    );
    // An allowlist narrows the surface, and the denylist is applied last, so
    // a name both lists match is withheld.
    assert_eq!(
        published_under(&["serena_*", "read_file"], &[]),
        ["read_file", "serena_find", "serena_replace"]
    );
    assert_eq!(
        published_under(&["serena_*"], &["serena_find"]),
        ["serena_replace"]
    );
}

/// Reference `available_tools` gates on the written list rather than on the
/// patterns it compiles to (`vibe/core/tools/manager.py:311`), so a session
/// that writes `enabled_tools` at all narrows the surface even when nothing in
/// it can ever match. The gate is decided again in `SessionToolExecutor::new`,
/// which is the copy a session actually publishes through.
#[test]
fn an_enabled_list_that_matches_nothing_still_publishes_nothing() {
    let every = [
        "read_file",
        "serena_find",
        "serena_replace",
        "web_fetch",
        "web_search",
    ];
    // No list written at all is the only way to publish everything.
    assert_eq!(published_under(&[], &[]), every);
    // Blank entries compile to no pattern, and an expression that does not
    // compile matches nothing: written, both still close the surface.
    assert!(published_under(&["  "], &[]).is_empty());
    assert!(published_under(&["re:("], &[]).is_empty());
    assert!(published_under(&["", "re:("], &[]).is_empty());
    // A blank entry alongside a usable one leaves the usable one deciding.
    assert_eq!(
        published_under(&["  ", "web_*"], &[]),
        ["web_fetch", "web_search"]
    );
    // `disabled_tools` withholds by match alone, so an unusable entry there
    // withholds nothing.
    assert_eq!(published_under(&[], &["  "]), every);
    assert_eq!(published_under(&[], &["re:("]), every);
}

/// The same rules guard execution, so a name the model remembers from an
/// earlier turn cannot be called once a pattern covers it.
#[tokio::test]
async fn a_pattern_that_hides_a_tool_also_refuses_to_execute_it() {
    let executor = SessionToolExecutor::new(
        filtering_registry(),
        &SessionIntent {
            disabled_tools: vec!["re:SERENA_.*".to_owned()],
            ..SessionIntent::default()
        },
    );
    let error = executor
        .execute("serena_find", "{}")
        .await
        .expect_err("a tool a pattern hides cannot execute");
    assert!(error.contains("disabled for this session"), "{error}");
    assert!(executor.execute("read_file", "{}").await.is_ok());
}

/// An entry that does not compile is dropped rather than applied, so one
/// mistyped expression cannot empty the surface.
#[test]
fn an_uncompilable_entry_leaves_the_rest_of_the_list_in_force() {
    assert_eq!(
        published_under(&[], &["re:[", "serena_*"]),
        ["read_file", "web_fetch", "web_search"]
    );
}

#[tokio::test]
async fn session_tool_filters_apply_again_at_execution_time() {
    let executions = Arc::new(AtomicUsize::new(0));
    let handler_executions = Arc::clone(&executions);
    let tools = ToolRegistry::default();
    tools
        .register(
            ToolSpec {
                name: "mcp_fixture_echo".to_owned(),
                description: "Echo through MCP".to_owned(),
                input_schema: ObjectSchema::new()
                    .required("message", Property::string())
                    .build(),
                output_schema: None,
                config: Value::Null,
                state: Value::Null,
                availability: ToolAvailability::Available,
                presentation: ToolPresentationKind::Mcp,
                source: ToolSource::Mcp,
                selection_priority: 50,
            },
            Arc::new(
                move |_invocation: &vibe_core::tools::ToolInvocation,
                      _output: vibe_core::tools::ToolOutputSink|
                      -> vibe_core::tools::OwnedToolHandlerFuture {
                    let executions = Arc::clone(&handler_executions);
                    Box::pin(async move {
                        executions.fetch_add(1, Ordering::AcqRel);
                        Ok(ToolExecutionOutput::text("unexpected execution"))
                    })
                },
            ),
        )
        .expect("register test MCP tool");
    let executor = SessionToolExecutor::new(
        tools,
        &SessionIntent {
            disabled_tools: vec!["mcp_fixture_echo".to_owned()],
            ..SessionIntent::default()
        },
    );
    let error = executor
        .execute("mcp_fixture_echo", r#"{"message":"rust"}"#)
        .await
        .expect_err("disabled tool cannot execute");
    assert!(error.contains("disabled for this session"));
    assert_eq!(executions.load(Ordering::Acquire), 0);
}

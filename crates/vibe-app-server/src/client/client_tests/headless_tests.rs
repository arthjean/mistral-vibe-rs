//! What a session declared headless carries into the turn it runs.

use super::*;

/// The flag a launch sets on `session/start` reaches the preamble the model
/// reads, and a launch that leaves it alone sends nothing.
///
/// The reference carries the same flag from the session options through the
/// runtime into the composed prompt (`vibe/app_server/_runtime.py:152` and
/// `:207`), so the directive is decided by the launch rather than by the
/// client that happens to compose a prompt.
#[tokio::test]
async fn a_headless_launch_states_the_directive_and_an_interactive_one_does_not() {
    for headless in [true, false] {
        let temporary = tempfile::tempdir().expect("temporary session root");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let driver = LiveTurnDriver::from_provider_for_tests(
            Arc::new(RecordingProvider {
                seen: Arc::clone(&seen),
            }),
            "current system",
        )
        .with_session_root_for_tests(Some(temporary.path().to_path_buf()));
        let mut service = HeadlessService::new(driver).expect("service starts");
        let mut launch = options();
        launch.headless = headless;
        launch.working_directory = temporary.path().to_string_lossy().into_owned();
        launch.session_id = Some("headless-probe".to_owned());
        launch.add_directories.clear();
        launch.tool_filters.clear();
        launch.enabled_tools.clear();
        launch.disabled_tools.clear();
        launch.agent = None;
        let session_id = service.start_session(&launch).expect("session starts");
        service.prompt(&session_id, "run it").await.expect("turn");

        let seen = seen.lock().expect("seen messages");
        assert_eq!(
            seen.iter()
                .filter(|message| matches!(
                    message,
                    ModelMessage::System { content }
                        if content == vibe_core::prompt::HEADLESS_SECTION
                ))
                .count(),
            usize::from(headless),
            "a launch with headless={headless} sent: {seen:?}"
        );
    }
}

/// A headless launch that also names one of the withheld tools in its
/// allowlist still does not publish it.
///
/// The reference narrows with `enabled_tools` and applies `disabled_tools`
/// last, so a name both lists match is withheld
/// (`vibe/core/tools/manager.py:311-322`, and the `both-lists-match` case of
/// `tests/tool-surface/gates.json` records the answer). The programmatic
/// launch relies on that order: it appends the two names to whatever the user
/// passed rather than editing the allowlist.
#[tokio::test]
async fn the_withheld_tools_stay_withheld_when_the_allowlist_names_one() {
    struct ToolRecordingProvider {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl CompletionProvider for ToolRecordingProvider {
        fn complete<'a>(
            &'a self,
            input: &'a ProviderInput,
        ) -> vibe_core::engine::ProviderFuture<'a> {
            Box::pin(async move {
                *self.seen.lock().map_err(|_| {
                    vibe_core::provider::ProviderError::MalformedStream(
                        "test lock poisoned".to_owned(),
                    )
                })? = input
                    .tools
                    .iter()
                    .map(|tool| tool.name.clone())
                    .collect::<Vec<_>>();
                Ok(AssistantMessage {
                    text: "done".to_owned(),
                    reasoning: None,
                    reasoning_signature: None,
                    reasoning_state: Vec::new(),
                    tool_calls: Vec::new(),
                    usage: Usage {
                        input_tokens: 1,
                        output_tokens: 1,
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
        Arc::new(ToolRecordingProvider {
            seen: Arc::clone(&seen),
        }),
        "current system",
    )
    .with_session_root_for_tests(Some(temporary.path().to_path_buf()));
    let mut service = HeadlessService::new(driver).expect("service starts");
    let mut launch = options();
    launch.headless = true;
    launch.working_directory = temporary.path().to_string_lossy().into_owned();
    launch.session_id = Some("allowlisted-question".to_owned());
    launch.add_directories.clear();
    launch.tool_filters.clear();
    launch.agent = None;
    launch.enabled_tools = vec!["ask_user_question".to_owned(), "read_file".to_owned()];
    launch.disabled_tools = vec!["ask_user_question".to_owned(), "exit_plan_mode".to_owned()];
    let session_id = service.start_session(&launch).expect("session starts");
    service.prompt(&session_id, "run it").await.expect("turn");

    let seen = seen.lock().expect("seen tools");
    assert!(
        !seen.iter().any(|name| name == "ask_user_question"),
        "the denylist is final: {seen:?}"
    );
    assert!(
        seen.iter().any(|name| name == "read_file"),
        "the allowlist still publishes what it names: {seen:?}"
    );
}

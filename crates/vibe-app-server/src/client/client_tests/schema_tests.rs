//! The tool schemas the interactive surface publishes, and the failure
//! vocabulary a driver classifies into.

use super::*;

/// A turn error is classified from the failure's type, so rewording a
/// message never moves the code a client branches on.
#[test]
fn driver_failures_classify_into_the_reference_error_vocabulary() {
    for (error, expected) in [
        (
            DriverError::Provider(ProviderError::ContextOverflow),
            TurnErrorCode::ContextTooLong,
        ),
        (
            DriverError::Provider(ProviderError::Refusal("no".to_owned())),
            TurnErrorCode::Refusal,
        ),
        (
            DriverError::Provider(ProviderError::HttpStatus { status: 429 }),
            TurnErrorCode::RateLimit,
        ),
        (
            DriverError::Provider(ProviderError::HttpStatus { status: 503 }),
            TurnErrorCode::BackendError,
        ),
        (
            DriverError::Provider(ProviderError::Transport(TransportError::ResponseTooLarge {
                limit: 8,
            })),
            TurnErrorCode::ResponseTooLong,
        ),
        (
            DriverError::ImageAttachment("not an image".to_owned()),
            TurnErrorCode::InvalidImageAttachment,
        ),
        (
            DriverError::Compaction("no summary".to_owned()),
            TurnErrorCode::CompactionFailed,
        ),
        (
            DriverError::Engine(EngineError::Compaction("no summary".to_owned())),
            TurnErrorCode::CompactionFailed,
        ),
        (DriverError::StatePoisoned, TurnErrorCode::InternalError),
    ] {
        assert_eq!(
            turn_error_code(&error),
            expected,
            "`{error}` classified wrongly"
        );
    }
}

/// The two argument conventions the reference publishes side by side.
///
/// `ask_user_question` takes its model from `UserQuestionRequest`, the one
/// reference argument model configuring `alias_generator=to_camel`, so its
/// properties are camelCase. Every other reference tool stays snake_case,
/// and both conventions must coexist in one published surface.
#[test]
fn the_interactive_schema_is_camel_case_while_the_file_tools_stay_snake_case() {
    let questions = interactive_question_spec().input_schema;
    let question = &questions["$defs"]["UserQuestion"]["properties"];
    for camel in ["multiSelect", "hideOther"] {
        assert!(question.get(camel).is_some(), "missing `{camel}`");
    }
    assert!(questions["properties"].get("footerNote").is_some());

    // `footerNote` is nullable through `anyOf`, never an array-form type.
    let footer = &questions["properties"]["footerNote"];
    assert_eq!(
        footer["anyOf"],
        json!([{"type": "string"}, {"type": "null"}])
    );
    assert_eq!(footer["default"], Value::Null);
    assert!(footer.get("type").is_none());

    // No `minLength` survives: the reference publishes none.
    assert!(
        !questions.to_string().contains("minLength"),
        "the reference publishes no minLength on this schema"
    );

    // The reference defaults reach the model as published values.
    assert_eq!(question["header"]["default"], "");
    assert_eq!(question["multiSelect"]["default"], false);
    assert_eq!(question["hideOther"]["default"], false);
    assert_eq!(
        questions["$defs"]["QuestionChoice"]["properties"]["description"]["default"],
        ""
    );

    // The same session publishes snake_case argument keys elsewhere.
    let directory = tempfile::tempdir().expect("workspace");
    let workspace =
        Arc::new(vibe_core::workspace::Workspace::open(directory.path()).expect("workspace"));
    let review = Arc::new(vibe_core::workspace::ReviewManager::new(workspace.clone()));
    let tools = ToolRegistry::default();
    vibe_core::workspace::WorkspaceTools::new(workspace, review)
        .register(
            &tools,
            &vibe_core::policy::ToolGuard::new(
                vibe_core::policy::PermissionStore::default(),
                Arc::new(DenyEveryApproval),
            ),
        )
        .expect("workspace tools register");
    let edit = tools
        .list()
        .expect("tools list")
        .into_iter()
        .find(|spec| spec.name == "edit")
        .expect("edit is published");
    for snake in ["file_path", "old_string", "new_string", "replace_all"] {
        assert!(
            edit.input_schema["properties"].get(snake).is_some(),
            "missing `{snake}`"
        );
    }
}

/// `exit_plan_mode` takes no arguments, and the reference publishes that
/// as two keys: no `required`, no `additionalProperties`.
#[test]
fn the_plan_review_schema_is_the_bare_reference_object() {
    assert_eq!(
        interactive_plan_review_spec().input_schema,
        json!({"type": "object", "properties": {}})
    );
}

/// `minItems: 2` on the options array is what makes an under-specified
/// question fail, and the failure names the question that caused it.
#[tokio::test]
async fn a_question_with_a_single_option_fails_naming_its_index() {
    let (sender, _receiver) = tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(1);
    let tools = ToolRegistry::default();
    InteractiveSessionToolFactory {
        sender,
        plan_directory: None,
    }
    .register("session", &tools)
    .expect("question tool registers");

    let error = tools
        .invoke(
            "ask_user_question",
            ToolInvocation {
                call_id: "question-1".to_owned(),
                arguments: json!({
                    "questions": [
                        {"question": "ok?", "options": [{"label": "a"}, {"label": "b"}]},
                        {"question": "which?", "options": [{"label": "only"}]},
                    ]
                }),
            },
        )
        .await
        .expect_err("a single-option question is under-specified");

    assert!(
        error.to_string().contains("$.questions[1].options"),
        "the failure must name the offending question: {error}"
    );
}

//! Plan mode: where the plan file may live, what the review callback exposes,
//! and what accepting a plan raises before the tool answers.

use super::*;

#[test]
fn plan_file_names_cannot_escape_the_plan_directory() {
    let path = plan_file_path(Path::new("/runtime/plans"), "session/../../outside");
    assert_eq!(path, Path::new("/runtime/plans/session_______outside.md"));
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some("session_______outside.md")
    );
    assert_eq!(
        path.parent()
            .and_then(|parent| parent.file_name())
            .and_then(|name| name.to_str()),
        Some("plans")
    );
}

#[tokio::test]
async fn plan_review_callback_exposes_the_live_driver_path() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(1);
    let plan_path = PathBuf::from("/runtime/plans/session.md");
    let task = tokio::spawn(run_interactive_plan_review(
        sender,
        "session".to_owned(),
        plan_path.clone(),
    ));
    let request = receiver.recv().await.expect("plan review request");
    assert!(matches!(request, InteractiveCallbackRequest::Tool { .. }));
    let InteractiveCallbackRequest::Tool {
        detail, response, ..
    } = request
    else {
        return;
    };
    assert_eq!(detail["filePath"], json!(plan_path));
    response
        .send(Ok(json!({
            "type": "user_input",
            "result": {
                "answers": [],
                "cancelled": true,
            },
        })))
        .expect("plan review response");
    task.await
        .expect("plan review task")
        .expect("plan review completes");
}

/// Accepting a plan with the clearing option raises the clearing on the
/// running turn, and the tool only answers once the turn holds it.
#[tokio::test]
async fn accepting_a_plan_with_clearing_raises_it_before_the_tool_answers() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(2);
    let plan_path = PathBuf::from("/runtime/plans/session.md");
    let task = tokio::spawn(run_interactive_plan_review(
        sender,
        "session".to_owned(),
        plan_path.clone(),
    ));
    let request = receiver.recv().await.expect("plan review request");
    assert!(matches!(request, InteractiveCallbackRequest::Tool { .. }));
    let InteractiveCallbackRequest::Tool { response, .. } = request else {
        return;
    };
    response
        .send(Ok(json!({
            "type": "user_input",
            "result": {
                "answers": [{
                    "question": "Plan is complete. Switch to code mode and start implementing?",
                    "answer": "Yes, clear context and auto approve edits",
                    "isOther": false,
                }],
                "cancelled": false,
            },
        })))
        .expect("plan review response");

    let raised = receiver.recv().await.expect("clearing request");
    assert!(
        matches!(raised, InteractiveCallbackRequest::ClearContext { .. }),
        "accepting with clearing raises a context clearing"
    );
    let InteractiveCallbackRequest::ClearContext {
        session_id,
        continuation,
        plan_file_path,
        response,
    } = raised
    else {
        return;
    };
    assert_eq!(session_id, "session");
    assert_eq!(plan_file_path.as_deref(), plan_path.to_str());
    assert!(
        continuation.contains("clear planning context"),
        "the continuation is the instruction the cleared turn restarts from: {continuation}"
    );
    response.send(Ok(())).expect("clearing acknowledgment");

    let output = task
        .await
        .expect("plan review task")
        .expect("plan review completes");
    assert_eq!(output.typed_result["switched"], true);
}

/// The other accepting option changes the session settings without touching
/// the transcript, so no clearing crosses the channel.
#[tokio::test]
async fn accepting_a_plan_without_clearing_raises_no_clearing() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(2);
    let task = tokio::spawn(run_interactive_plan_review(
        sender,
        "session".to_owned(),
        PathBuf::from("/runtime/plans/session.md"),
    ));
    let request = receiver.recv().await.expect("plan review request");
    assert!(matches!(request, InteractiveCallbackRequest::Tool { .. }));
    let InteractiveCallbackRequest::Tool { response, .. } = request else {
        return;
    };
    response
        .send(Ok(json!({
            "type": "user_input",
            "result": {
                "answers": [{
                    "question": "Plan is complete. Switch to code mode and start implementing?",
                    "answer": "Yes, and auto approve edits",
                    "isOther": false,
                }],
                "cancelled": false,
            },
        })))
        .expect("plan review response");
    task.await
        .expect("plan review task")
        .expect("plan review completes");
    assert!(
        receiver.try_recv().is_err(),
        "only the clearing option clears the context"
    );
}

//! Plan mode: where the plan file may live, what the review callback exposes,
//! and what accepting a plan raises before the tool answers.

use super::*;

#[test]
fn a_plan_file_is_named_once_per_session_and_stays_in_the_plan_directory() {
    let first = plan_file_path(Path::new("/runtime/plans"), "session/../../outside");
    let again = plan_file_path(Path::new("/runtime/plans"), "session/../../outside");
    assert_eq!(first, again, "a session keeps the plan file it was given");
    assert_eq!(first.parent(), Some(Path::new("/runtime/plans")));
    let name = first
        .file_name()
        .and_then(|name| name.to_str())
        .expect("the plan file has a name");
    // Reference `PlanSession.plan_file_path`: a Unix timestamp, then a slug.
    let (stamp, slug) = name
        .strip_suffix(".md")
        .and_then(|stem| stem.split_once('-'))
        .expect("a timestamp and a slug");
    assert!(stamp.chars().all(|character| character.is_ascii_digit()));
    assert_eq!(slug.split('-').count(), 3, "{slug}");
    assert!(
        slug.chars()
            .all(|character| character.is_ascii_lowercase() || character == '-'),
        "{slug}"
    );
    assert_ne!(
        first,
        plan_file_path(Path::new("/runtime/plans"), "another-session"),
        "another session draws its own name"
    );
}

/// Answers the agent lookup the review opens with, as a session that runs
/// `agent`.
async fn answer_active_agent(
    receiver: &mut tokio::sync::mpsc::Receiver<InteractiveCallbackRequest>,
    agent: Option<&str>,
) {
    let request = receiver.recv().await.expect("agent lookup");
    assert!(matches!(
        request,
        InteractiveCallbackRequest::ActiveAgent { .. }
    ));
    let InteractiveCallbackRequest::ActiveAgent { response, .. } = request else {
        return;
    };
    response
        .send(agent.map(str::to_owned))
        .expect("agent lookup answer");
}

/// Answers the review's question with `answer`, returning the callback detail.
async fn answer_review(
    receiver: &mut tokio::sync::mpsc::Receiver<InteractiveCallbackRequest>,
    answer: &str,
) -> Value {
    let request = receiver.recv().await.expect("plan review request");
    assert!(matches!(request, InteractiveCallbackRequest::Tool { .. }));
    let InteractiveCallbackRequest::Tool {
        detail, response, ..
    } = request
    else {
        return Value::Null;
    };
    let question = detail
        .pointer("/request/questions/0/question")
        .cloned()
        .unwrap_or_default();
    response
        .send(Ok(json!({
            "type": "user_input",
            "result": {
                "answers": [{"question": question, "answer": answer, "isOther": false}],
                "cancelled": false,
            },
        })))
        .expect("plan review response");
    detail
}

/// Acknowledges the agent switch the review raises, returning its target.
async fn acknowledge_switch(
    receiver: &mut tokio::sync::mpsc::Receiver<InteractiveCallbackRequest>,
) -> String {
    let request = receiver.recv().await.expect("agent switch");
    assert!(matches!(
        request,
        InteractiveCallbackRequest::SwitchAgent { .. }
    ));
    let InteractiveCallbackRequest::SwitchAgent {
        agent_name,
        response,
        ..
    } = request
    else {
        return String::new();
    };
    response.send(Ok(())).expect("switch acknowledgment");
    agent_name
}

#[tokio::test]
async fn plan_review_callback_exposes_the_live_driver_path() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(1);
    let plan_path = PathBuf::from("/runtime/plans/session.md");
    let task = tokio::spawn(run_interactive_plan_review(
        crate::client::interactive::CallbackChannel::Shared(sender),
        "session".to_owned(),
        String::new(),
        plan_path.clone(),
    ));
    answer_active_agent(&mut receiver, Some("plan")).await;
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
    let output = task
        .await
        .expect("plan review task")
        .expect("plan review completes");
    assert_eq!(output.typed_result["switched"], false);
}

/// Reference `ExitPlanMode.run` refuses outside the plan profile before it
/// asks the user anything.
#[tokio::test]
async fn a_plan_review_outside_plan_mode_is_refused_before_asking() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(1);
    let task = tokio::spawn(run_interactive_plan_review(
        crate::client::interactive::CallbackChannel::Shared(sender),
        "session".to_owned(),
        String::new(),
        PathBuf::from("/runtime/plans/session.md"),
    ));
    answer_active_agent(&mut receiver, Some("accept-edits")).await;
    assert!(
        task.await.expect("plan review task").is_err(),
        "a session outside plan mode cannot leave it"
    );
    assert!(receiver.try_recv().is_err(), "nothing was asked");
}

/// Accepting a plan with the clearing option moves the session to
/// `accept-edits`, then raises the clearing on the running turn, re-seeded
/// with the approved plan, and the tool only answers once the turn holds it.
#[tokio::test]
async fn accepting_a_plan_with_clearing_raises_it_before_the_tool_answers() {
    let directory = tempfile::tempdir().expect("plan directory");
    let plan_path = directory.path().join("plan.md");
    std::fs::write(&plan_path, "1. Do the thing\n").expect("plan written");
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(2);
    let task = tokio::spawn(run_interactive_plan_review(
        crate::client::interactive::CallbackChannel::Shared(sender),
        "session".to_owned(),
        String::new(),
        plan_path.clone(),
    ));
    answer_active_agent(&mut receiver, Some("plan")).await;
    answer_review(&mut receiver, "Yes, clear context and auto approve edits").await;
    assert_eq!(acknowledge_switch(&mut receiver).await, "accept-edits");

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
        continuation.contains("1. Do the thing"),
        "the cleared turn restarts from the approved plan: {continuation}"
    );
    response.send(Ok(())).expect("clearing acknowledgment");

    let output = task
        .await
        .expect("plan review task")
        .expect("plan review completes");
    assert_eq!(output.typed_result["switched"], true);
}

/// A clearing with no plan written leaves nothing but the system prompt,
/// and names no plan file.
#[tokio::test]
async fn clearing_without_a_plan_restarts_from_nothing() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(2);
    let task = tokio::spawn(run_interactive_plan_review(
        crate::client::interactive::CallbackChannel::Shared(sender),
        "session".to_owned(),
        String::new(),
        PathBuf::from("/runtime/plans/missing.md"),
    ));
    answer_active_agent(&mut receiver, Some("plan")).await;
    answer_review(&mut receiver, "Yes, clear context and auto approve edits").await;
    acknowledge_switch(&mut receiver).await;
    let raised = receiver.recv().await.expect("clearing request");
    assert!(
        matches!(raised, InteractiveCallbackRequest::ClearContext { .. }),
        "accepting with clearing raises a context clearing"
    );
    let InteractiveCallbackRequest::ClearContext {
        continuation,
        plan_file_path,
        response,
        ..
    } = raised
    else {
        return;
    };
    assert!(continuation.is_empty());
    assert_eq!(plan_file_path, None);
    response.send(Ok(())).expect("clearing acknowledgment");
    task.await
        .expect("plan review task")
        .expect("plan review completes");
}

/// The other accepting options switch the profile without touching the
/// transcript, so no clearing crosses the channel.
#[tokio::test]
async fn accepting_a_plan_without_clearing_raises_no_clearing() {
    for (answer, target) in [
        ("Yes, and auto approve edits", "accept-edits"),
        ("Yes, and request approval for edits", "ask"),
    ] {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(2);
        let task = tokio::spawn(run_interactive_plan_review(
            crate::client::interactive::CallbackChannel::Shared(sender),
            "session".to_owned(),
            String::new(),
            PathBuf::from("/runtime/plans/session.md"),
        ));
        answer_active_agent(&mut receiver, Some("plan")).await;
        answer_review(&mut receiver, answer).await;
        assert_eq!(acknowledge_switch(&mut receiver).await, target);
        let output = task
            .await
            .expect("plan review task")
            .expect("plan review completes");
        assert_eq!(output.typed_result["switched"], true);
        assert!(
            receiver.try_recv().is_err(),
            "only the clearing option clears the context"
        );
    }
}

/// Staying in plan mode switches nothing.
#[tokio::test]
async fn declining_a_plan_stays_in_plan_mode() {
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<InteractiveCallbackRequest>(2);
    let task = tokio::spawn(run_interactive_plan_review(
        crate::client::interactive::CallbackChannel::Shared(sender),
        "session".to_owned(),
        String::new(),
        PathBuf::from("/runtime/plans/session.md"),
    ));
    answer_active_agent(&mut receiver, Some("plan")).await;
    answer_review(&mut receiver, "No").await;
    let output = task
        .await
        .expect("plan review task")
        .expect("plan review completes");
    assert_eq!(output.typed_result["switched"], false);
    assert_eq!(output.display["success"], false);
    assert!(receiver.try_recv().is_err(), "nothing is switched");
}

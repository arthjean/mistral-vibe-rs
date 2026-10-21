//! The `exit_plan_mode` tool: the verdict the user gives on a finished plan,
//! and the profile switch and context clearing an approval hands the turn.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use vibe_core::schema::ObjectSchema;
use vibe_core::tools::{
    ToolAvailability, ToolError, ToolExecutionOutput, ToolPresentationKind, ToolSource, ToolSpec,
    reference_text,
};

use super::questions::user_input_result;
use super::{
    CallbackChannel, InteractiveCallbackRequest, related_entry_id,
    request_interactive_tool_callback,
};

/// Asks the surface driving the turn to clear the transcript and rotate the
/// session, and waits for it to be queued.
///
/// Waiting is what makes the answer honest: the tool reports a cleared context
/// only once the turn actually holds the control that clears it.
async fn request_context_clearing(
    sender: CallbackChannel,
    session_id: String,
    continuation: String,
    plan_file_path: Option<String>,
) -> Result<(), ToolError> {
    let (response, receiver) = tokio::sync::oneshot::channel();
    sender
        .send(InteractiveCallbackRequest::ClearContext {
            session_id,
            continuation,
            plan_file_path,
            response,
        })
        .await
        .map_err(|_| ToolError::Execution("interactive callback queue closed".to_owned()))?;
    receiver
        .await
        .map_err(|_| ToolError::Execution("context clearing was abandoned".to_owned()))?
        .map_err(ToolError::Execution)
}

/// Asks the surface driving the turn which agent profile the session runs.
async fn request_active_agent(
    sender: &CallbackChannel,
    session_id: String,
) -> Result<Option<String>, ToolError> {
    let (response, receiver) = tokio::sync::oneshot::channel();
    sender
        .send(InteractiveCallbackRequest::ActiveAgent {
            session_id,
            response,
        })
        .await
        .map_err(|_| ToolError::Execution("interactive callback queue closed".to_owned()))?;
    receiver
        .await
        .map_err(|_| ToolError::Execution("agent lookup was abandoned".to_owned()))
}

/// Moves the session to `agent_name` and waits until the running turn holds
/// the switch, so the tool never reports a profile the next request lacks.
async fn request_agent_switch(
    sender: &CallbackChannel,
    session_id: String,
    agent_name: &str,
) -> Result<(), ToolError> {
    let (response, receiver) = tokio::sync::oneshot::channel();
    sender
        .send(InteractiveCallbackRequest::SwitchAgent {
            session_id,
            agent_name: agent_name.to_owned(),
            response,
        })
        .await
        .map_err(|_| ToolError::Execution("interactive callback queue closed".to_owned()))?;
    receiver
        .await
        .map_err(|_| ToolError::Execution("agent switch was abandoned".to_owned()))?
        .map_err(ToolError::Execution)
}

/// The labels a client answers the plan review with. They are the answer
/// values the reference matches on (`vibe/core/tools/builtins/exit_plan_mode.py`),
/// so they are the protocol rather than prose.
const PLAN_CLEAR_AUTO: &str = "Yes, clear context and auto approve edits";
const PLAN_AUTO: &str = "Yes, and auto approve edits";
const PLAN_MANUAL: &str = "Yes, and request approval for edits";
const PLAN_STAY: &str = "No";

/// The profile a plan review may only run under, the builtin plan agent
/// (reference `BuiltinAgentName.PLAN`), and the two it hands off to.
pub(crate) const PLAN_AGENT: &str = "plan";
const EDITING_AGENT: &str = "accept-edits";
const ASKING_AGENT: &str = "ask";

pub(crate) async fn run_interactive_plan_review(
    sender: CallbackChannel,
    session_id: String,
    call_id: String,
    plan_path: PathBuf,
) -> Result<ToolExecutionOutput, ToolError> {
    const QUESTION: &str = "Plan is complete. Switch to code mode and start implementing?";
    // Reference `PlanSession.snapshot_content_hash`, taken when the call
    // starts: what the plan read before the user was asked about it.
    let reviewed = read_plan(&plan_path).await;
    // Reference `ExitPlanMode.run` refuses outside the plan profile before it
    // asks anything, so a profile that enables the tool cannot leave a mode it
    // is not in.
    if request_active_agent(&sender, session_id.clone())
        .await?
        .as_deref()
        != Some(PLAN_AGENT)
    {
        return Err(ToolError::Execution(
            "exit_plan_mode only answers while the session runs the plan agent".to_owned(),
        ));
    }
    let detail = json!({
        "kind": "user_input",
        "request": {
            "questions": [{
                "question": QUESTION,
                "header": "Plan ready",
                "options": interactive_plan_options(),
                "multiSelect": false,
                "hideOther": false,
            }],
            "footerNote": format!(
                "Plan file: {} (Ctrl+G opens it for editing)",
                plan_path.display()
            ),
        },
        "planReview": true,
        "filePath": plan_path,
        "relatedEntryId": related_entry_id(call_id),
    });
    let output =
        request_interactive_tool_callback(sender.clone(), session_id.clone(), detail).await?;
    let (answers, cancelled) = user_input_result(&output)?;
    // Reference `ExitPlanMode.run` matches the first answer's label without
    // regard to case, reads a free-text answer as feedback, and stays in plan
    // mode for anything else.
    let first = answers.first().filter(|_| !cancelled);
    let verdict = first.map(|answer| {
        let value = answer
            .get("answer")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let other = answer.get("isOther").and_then(Value::as_bool) == Some(true);
        (value.to_lowercase(), value.to_owned(), other)
    });
    let (target, clear_context, message) = match &verdict {
        None => (
            None,
            false,
            "User cancelled. Staying in plan mode.".to_owned(),
        ),
        Some((lower, _, _)) if lower == &PLAN_CLEAR_AUTO.to_lowercase() => (
            Some(EDITING_AGENT),
            true,
            "Now in accept-edits mode. The planning context is being cleared, and \
             implementation starts from the approved plan."
                .to_owned(),
        ),
        Some((lower, _, _)) if lower == &PLAN_AUTO.to_lowercase() => (
            Some(EDITING_AGENT),
            false,
            "Now in accept-edits mode: go ahead and implement the plan.".to_owned(),
        ),
        Some((lower, _, _)) if lower == &PLAN_MANUAL.to_lowercase() => (
            Some(ASKING_AGENT),
            false,
            "Now in ask mode: every edit waits for the user's approval.".to_owned(),
        ),
        Some((_, value, true)) => (
            None,
            false,
            format!("Still planning. The user's feedback: {value}"),
        ),
        Some(_) => (
            None,
            false,
            "Still planning: keep refining the plan.".to_owned(),
        ),
    };
    if let Some(target) = target {
        request_agent_switch(&sender, session_id.clone(), target).await?;
    }
    if clear_context {
        // Reference `_clear_context_after_plan_accept` re-seeds the cleared
        // conversation with the approved plan, and with nothing when the plan
        // file is empty or missing, which is also when no path is named.
        let plan = tokio::fs::read_to_string(&plan_path)
            .await
            .unwrap_or_default();
        let (continuation, plan_file_path) = if plan.is_empty() {
            (String::new(), None)
        } else {
            (
                format!(
                    "<{tag}>The conversation was cleared once the plan was approved. \
                     Carry out the approved plan below; it is authoritative.\n\n{plan}</{tag}>",
                    tag = vibe_core::workspace::WARNING_TAG
                ),
                plan_path.to_str().map(str::to_owned),
            )
        };
        request_context_clearing(sender, session_id, continuation, plan_file_path).await?;
    }
    let switched = target.is_some();
    // Reference `_handle_plan_review_ended`: a plan the user edited while it
    // was under review reaches the model, as the version to implement, once
    // the step settles.
    let pending_injection = match read_plan(&plan_path).await {
        Some(plan) if Some(&plan) != reviewed.as_ref() => Some(format!(
            "<{tag}>The plan file was edited by hand while it was under review. \
             Implement the edited version below; it supersedes the one you wrote.\n\n{plan}</{tag}>",
            tag = vibe_core::workspace::WARNING_TAG
        )),
        _ => None,
    };
    // Reference `ExitPlanModeResult`
    // (`vibe/core/tools/builtins/exit_plan_mode.py:34`) declares `switched` then
    // `message`, and the agent loop renders one field per line, so the decision
    // reaches the model beside the sentence rather than only inside it.
    let model_text = reference_text::joined(&[
        ("switched", reference_text::boolean(switched).to_owned()),
        ("message", message.clone()),
    ]);
    Ok(ToolExecutionOutput {
        skip: None,
        turn_failure: None,
        // Reference `ExitPlanModeConfig.permission` is `always`, so the call
        // runs on the configuration's say.
        approval: Some(vibe_core::tools::ToolApproval {
            decision: vibe_core::tools::ToolVerdict::Execute,
            approval_type: vibe_core::tools::ToolApprovalType::Always,
            approval_source: vibe_core::tools::ToolApprovalSource::Config,
        }),
        failure: None,
        typed_result: json!({"switched": switched, "message": message}),
        model_text,
        // Reference `ExitPlanMode.format_result_display`: the verdict is the
        // header, and a plan that stays reads as unsuccessful.
        display: json!({
            "success": switched,
            "verb": "",
            "message": message,
            "warnings": [],
            "approvalNote": null,
            "suffix": "",
        }),
        projected_result: serde_json::Value::Null,
        chunks: Vec::new(),
        pending_injection,
        remote_result: None,
    })
}

/// Reference `PlanSession.read`: the plan's text, or nothing when the file
/// is absent.
async fn read_plan(plan_path: &Path) -> Option<String> {
    tokio::fs::read(plan_path)
        .await
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Where a session's plan lives: reference `PlanSession.plan_file_path`, a
/// Unix timestamp and a random slug drawn once per session and kept for the
/// life of the process.
pub(crate) fn plan_file_path(plan_directory: &Path, session_id: &str) -> PathBuf {
    static NAMES: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> =
        std::sync::OnceLock::new();
    let draw = || {
        format!(
            "{}-{}.md",
            vibe_core::clock::now_seconds(),
            vibe_core::worktree::random_slug()
        )
    };
    let name = crate::sync::lock(NAMES.get_or_init(Default::default))
        .entry(session_id.to_owned())
        .or_insert_with(draw)
        .clone();
    plan_directory.join(name)
}

fn interactive_plan_options() -> Vec<Value> {
    [
        (
            PLAN_CLEAR_AUTO,
            "Clear planning context, switch to code mode, and auto approve edits",
        ),
        (PLAN_AUTO, "Switch to code mode with auto-approved edits"),
        (PLAN_MANUAL, "Switch to code mode and keep edit approvals"),
        (PLAN_STAY, "Keep planning and refine the plan further"),
    ]
    .into_iter()
    .map(|(label, description)| json!({"label": label, "description": description}))
    .collect()
}

/// Directive coverage for `exit_plan_mode`.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | The call announces a finished plan ready to implement | "the plan is finished and ready to implement" |
/// | Call it only once the plan is final | "Call it only once the plan is final" |
/// | Do not call it while planning continues | "never while planning is still under way or the user wants to keep planning" |
pub(crate) fn interactive_plan_review_spec() -> ToolSpec {
    ToolSpec {
        name: "exit_plan_mode".to_owned(),
        description: "Announce that the plan is finished and ready to implement. Call it only \
                      once the plan is final, never while planning is still under way or the \
                      user wants to keep planning."
            .to_owned(),
        input_schema: ObjectSchema::new().build(),
        output_schema: None,
        config: Value::Null,
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Generic,
        source: ToolSource::BuiltIn,
        selection_priority: 50,
    }
}

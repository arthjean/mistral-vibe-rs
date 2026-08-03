//! Commands whose execution needs the live session runtime, executed after
//! [`super::dispatch_command`] has classified the submitted line.

use std::path::Path;

use serde_json::{Value, json};

use super::super::chat_input::ChatInputState;
use super::super::cloud_workflow::{format_cancelled_loop, format_created_loop, format_loop_list};
use super::super::controls::ControlState;
use super::super::remote_project_workflow::{handle_project_command, handle_teleport_command};
use super::super::setup::ResolvedTheme;
use super::super::state::{EntryStatus, TuiState};
use super::super::switching::{self, SwitchRequest};
use super::super::{
    InteractiveRuntime, adopt_hydrated_session, call_runtime, metadata_session_id,
    push_local_notice, unix_seconds, update_theme,
};
use super::RuntimeCommand;
use super::config::apply_thinking;

#[allow(clippy::too_many_arguments)]
pub(in crate::tui) async fn handle_runtime_command(
    command: &RuntimeCommand,
    working_directory: &Path,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
    controls: &mut ControlState,
    input: &mut ChatInputState,
    theme: &mut ResolvedTheme,
    turn_active: bool,
) {
    let Some(runtime) = runtime.as_mut() else {
        state.push_diagnostic("Setup is required before using session commands");
        return;
    };
    if turn_active && command.changes_session_projection() {
        state.push_diagnostic(
            "Finish or interrupt the active turn before changing the session projection",
        );
        return;
    }

    match command {
        RuntimeCommand::Clear => {
            if call_runtime(
                runtime,
                "session/history/clear",
                json!({"sessionId": runtime.session_id}),
                state,
            )
            .is_some()
            {
                let session_id = runtime.session_id.clone();
                if adopt_hydrated_session(runtime, state, controls, session_id) {
                    push_local_notice(
                        state,
                        "Conversation history cleared",
                        EntryStatus::Completed,
                    );
                }
            }
        }
        RuntimeCommand::Compact(instructions) => {
            let instructions = instructions.trim();
            let session_id = runtime.session_id.clone();
            match runtime.service.compact(&session_id, instructions).await {
                Ok(result) => {
                    let new_session_id = result
                        .get("state")
                        .and_then(|state| state.pointer("/session/id"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    let summary_length = result
                        .get("summary")
                        .and_then(Value::as_str)
                        .map_or(0, str::len);
                    match new_session_id {
                        Some(ref new_session_id)
                            if adopt_hydrated_session(
                                runtime,
                                state,
                                controls,
                                new_session_id.clone(),
                            ) =>
                        {
                            push_local_notice(
                                state,
                                &format!("Context compacted ({summary_length} summary bytes)"),
                                EntryStatus::Completed,
                            );
                        }
                        Some(_) => {}
                        None => state
                            .push_diagnostic("Compaction completed without a canonical session id"),
                    }
                }
                Err(error) => state.push_diagnostic(error.to_string()),
            }
        }
        RuntimeCommand::Rewind(keep) => {
            let keep_messages = keep
                .split_whitespace()
                .next()
                .map(str::parse::<usize>)
                .transpose();
            match keep_messages {
                Ok(keep_messages) => {
                    let keep_messages = keep_messages.unwrap_or_default();
                    if call_runtime(
                        runtime,
                        "session/rewind",
                        json!({
                            "sessionId": runtime.session_id,
                            "keepMessages": keep_messages,
                        }),
                        state,
                    )
                    .is_some()
                    {
                        let session_id = runtime.session_id.clone();
                        if adopt_hydrated_session(runtime, state, controls, session_id) {
                            push_local_notice(
                                state,
                                &format!("Rewound to {keep_messages} stored messages"),
                                EntryStatus::Completed,
                            );
                        }
                    }
                }
                Err(_) => state.push_diagnostic("Usage: /rewind [message-count]"),
            }
        }
        RuntimeCommand::Resume(requested) => {
            let Some(session_id) = requested.split_whitespace().next() else {
                state.push_diagnostic("Usage: /resume <session-id>");
                return;
            };
            if let Some(result) = call_runtime(
                runtime,
                "session/resume",
                json!({"sessionId": session_id}),
                state,
            ) && let Some(session_id) = metadata_session_id(&result)
                && adopt_hydrated_session(runtime, state, controls, session_id)
            {
                push_local_notice(state, "Resumed session", EntryStatus::Completed);
            }
        }
        RuntimeCommand::Rename(title) => {
            let title = title.trim();
            if title.is_empty() {
                state.push_diagnostic("Usage: /title <new title>");
            } else if call_runtime(
                runtime,
                "session/title/update",
                json!({"sessionId": runtime.session_id, "title": title}),
                state,
            )
            .is_some()
            {
                push_local_notice(state, "Session title updated", EntryStatus::Completed);
            }
        }
        RuntimeCommand::Model(model) => {
            switching::request(
                runtime,
                input,
                state,
                SwitchRequest::Model {
                    model: model.clone(),
                    target: None,
                },
            );
        }
        RuntimeCommand::Thinking(value) => apply_thinking(runtime, value, None, state),
        RuntimeCommand::Theme(value) => {
            let value = value.split_whitespace().next().unwrap_or_default();
            update_theme(runtime, value, None, state, theme);
        }
        RuntimeCommand::Loop(arguments) => handle_loop_command(arguments, runtime, state),
        RuntimeCommand::RemoteProject(arguments) => {
            handle_project_command(arguments, working_directory, runtime, state);
        }
        RuntimeCommand::Teleport(arguments) => {
            handle_teleport_command(arguments, working_directory, runtime, state);
        }
    }
}

fn handle_loop_command(arguments: &str, runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    let arguments = arguments.split_whitespace().collect::<Vec<_>>();
    match arguments.as_slice() {
        [] | ["list"] | ["ls"] => {
            if let Some(result) = call_runtime(
                runtime,
                "loops/list",
                json!({"sessionId": runtime.session_id}),
                state,
            ) {
                match result
                    .get("loops")
                    .ok_or("Scheduled-loop response omitted its list")
                    .and_then(|loops| format_loop_list(loops, unix_seconds()))
                {
                    Ok(message) => push_local_notice(state, &message, EntryStatus::Completed),
                    Err(message) => state.push_diagnostic(message),
                }
            }
        }
        [action, target] if matches!(*action, "cancel" | "rm" | "stop" | "delete") => {
            let (method, params) = if *target == "all" {
                ("loops/clear", json!({"sessionId": runtime.session_id}))
            } else {
                (
                    "loops/delete",
                    json!({"sessionId": runtime.session_id, "loopId": target}),
                )
            };
            if let Some(result) = call_runtime(runtime, method, params, state) {
                let message = if *target == "all" {
                    result
                        .get("count")
                        .and_then(Value::as_u64)
                        .map(|count| format!("Cancelled {count} scheduled loop(s)."))
                        .ok_or("Scheduled-loop clear response omitted its count")
                } else {
                    result
                        .get("loop")
                        .ok_or("Scheduled-loop delete response omitted the loop")
                        .and_then(format_cancelled_loop)
                };
                match message {
                    Ok(message) => push_local_notice(state, &message, EntryStatus::Completed),
                    Err(message) => state.push_diagnostic(message),
                }
            }
        }
        [interval, prompt @ ..] if !prompt.is_empty() => {
            if let Some(result) = call_runtime(
                runtime,
                "loops/create",
                json!({
                    "sessionId": runtime.session_id,
                    "interval": interval,
                    "prompt": prompt.join(" "),
                }),
                state,
            ) {
                match result
                    .get("loop")
                    .ok_or("Scheduled-loop create response omitted the loop")
                    .and_then(format_created_loop)
                {
                    Ok(message) => push_local_notice(state, &message, EntryStatus::Completed),
                    Err(message) => state.push_diagnostic(message),
                }
            }
        }
        _ => state.push_diagnostic(
            "Usage: /loop [list|ls] | /loop <interval> <prompt> | /loop delete <id|all>",
        ),
    }
}

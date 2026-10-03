//! What a Teleport run publishes, read into the transcript line the operator
//! sees. The server reports the run's telemetry itself.

use serde_json::Value;

use super::state::EntryStatus;

pub(super) fn teleport_event_message(
    event: Option<&Value>,
) -> Result<(String, EntryStatus), &'static str> {
    let event = event.ok_or("Teleport event omitted its payload")?;
    let kind = event
        .get("kind")
        .and_then(Value::as_str)
        .ok_or("Teleport event omitted its kind")?;
    let (message, status) = match kind {
        "summarizing_context" => ("Summarizing context...".to_owned(), EntryStatus::Streaming),
        "checking_git" => ("Preparing workspace...".to_owned(), EntryStatus::Streaming),
        "push_required" => {
            let count = event
                .get("unpushedCount")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let branch = event
                .get("branchNotPushed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let message = if branch {
                "Teleport requires publishing the current branch.".to_owned()
            } else {
                format!(
                    "Teleport requires pushing {count} commit{}.",
                    if count == 1 { "" } else { "s" }
                )
            };
            (message, EntryStatus::Streaming)
        }
        "pushing" => ("Syncing with remote...".to_owned(), EntryStatus::Streaming),
        "starting_workflow" => ("Teleporting...".to_owned(), EntryStatus::Streaming),
        "complete" => {
            let url = event
                .get("url")
                .and_then(Value::as_str)
                .ok_or("Completed Teleport event omitted its URL")?;
            (
                format!("Teleported to Vibe Code Web: {url}"),
                EntryStatus::Completed,
            )
        }
        "failed" => {
            let message = event
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("Teleport failed");
            return Ok((format!("Teleport failed: {message}"), EntryStatus::Failed));
        }
        "cancelled" => ("Teleport cancelled.".to_owned(), EntryStatus::Cancelled),
        _ => return Err("Teleport event kind is unknown"),
    };
    Ok((message, status))
}
#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn each_event_kind_reads_as_one_transcript_line() {
        let line = |event: Value| teleport_event_message(Some(&event)).expect("a known kind");
        assert_eq!(
            line(json!({"kind": "push_required", "unpushedCount": 1})),
            (
                "Teleport requires pushing 1 commit.".to_owned(),
                EntryStatus::Streaming
            )
        );
        assert_eq!(
            line(json!({"kind": "complete", "url": "https://example.test/s"})).1,
            EntryStatus::Completed
        );
        assert_eq!(
            line(json!({"kind": "failed", "error": {"message": "refused"}})),
            ("Teleport failed: refused".to_owned(), EntryStatus::Failed)
        );
        assert!(teleport_event_message(Some(&json!({"kind": "unknown"}))).is_err());
    }
}

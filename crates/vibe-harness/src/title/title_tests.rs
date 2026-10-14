use serde_json::json;

use super::*;

#[test]
fn a_title_is_due_once_early_then_after_six_more_steps() {
    let mut cadence = TitleCadence::default();
    assert!(!cadence.begin_if_due(1, None, None, false));
    assert!(cadence.begin_if_due(1, None, None, true));
    cadence.record("First");
    assert!(!cadence.begin_if_due(6, Some("First"), Some("checkpoint-1"), false));
    assert!(cadence.begin_if_due(7, Some("First"), None, false));
    assert!(!cadence.begin_if_due(20, Some("First"), None, true));

    let mut renamed = TitleCadence::default();
    assert!(!renamed.begin_if_due(5, Some("Mine"), None, true));
}

#[test]
fn an_answer_is_reduced_to_one_short_line() {
    assert_eq!(
        clean("  \"Parser   plan\"\nmore").as_deref(),
        Some("Parser plan")
    );
    assert_eq!(clean("New Session"), None);
    assert_eq!(clean(""), None);
    let long = "word ".repeat(30);
    let cleaned = clean(&long).unwrap();
    assert_eq!(cleaned.chars().count(), 73);
    assert!(cleaned.ends_with('…'));
}

#[test]
fn the_transcript_reads_messages_and_calls() {
    let entries = [
        json!({"type": "message", "role": "user", "content": [{"type": "text", "text": "Run it"}]}),
        json!({"type": "effect", "generationStatus": "completed",
               "detail": {"toolName": "file_system.bash", "input": {"command": "echo hi"}},
               "state": {"status": "completed", "outputText": "hi\n"}}),
        json!({"type": "message", "role": "assistant", "content": [{"type": "text", "text": "Done."}]}),
    ];
    assert_eq!(
        transcript(&entries),
        "user: Run it\n\ntool(file_system.bash): {\"command\": \"echo hi\"}\n\
         tool result(file_system.bash): hi\n\nassistant: Done."
    );
    assert_eq!(count_model_steps(&entries), 2);
}

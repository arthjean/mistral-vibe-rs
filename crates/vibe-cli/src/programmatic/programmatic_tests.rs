use serde_json::json;

use super::layout;
use super::pyjson::Ordered;

#[test]
fn an_effect_names_its_input_and_output_keys_in_declared_order() {
    let mut entry = Ordered::of(&json!({
        "type": "effect",
        "detail": {"kind": "file_read", "input": {"filePath": "a", "limit": 2000, "offset": null}},
        "state": {"output": {"content": "x", "filePath": "a", "numLines": 1, "startLine": 1}},
    }))
    .expect("entry");
    layout::arrange(&mut entry);
    assert_eq!(
        entry.compact(),
        r#"{"detail": {"input": {"filePath": "a", "offset": null, "limit": 2000}, "kind": "file_read"}, "state": {"output": {"filePath": "a", "content": "x", "numLines": 1, "startLine": 1}}, "type": "effect"}"#
    );
}

#[test]
fn a_kind_without_a_model_keeps_its_order() {
    let mut entry = Ordered::of(&json!({
        "type": "effect",
        "detail": {"kind": "tool", "input": {"b": 1, "a": 2}},
    }))
    .expect("entry");
    let before = entry.compact();
    layout::arrange(&mut entry);
    assert_eq!(entry.compact(), before);
}

#[test]
fn a_teleport_reports_progress_in_text_and_its_url_in_json() {
    use vibe_app_server::client::ProgrammaticTeleportEvent;

    let events = [
        ProgrammaticTeleportEvent::CheckingGit {
            operation_id: "teleport-1".to_owned(),
        },
        ProgrammaticTeleportEvent::Complete {
            operation_id: "teleport-1".to_owned(),
            url: "https://vibe.example/run".to_owned(),
        },
    ];
    let mut text = super::Output::new(crate::OutputMode::Text);
    let mut progress = Vec::new();
    for event in &events {
        text.teleport(event, &mut progress).expect("progress");
    }
    assert_eq!(
        String::from_utf8(progress).expect("UTF-8"),
        "Preparing workspace...\n"
    );
    assert_eq!(
        text.finalize(&[], &mut Vec::new()).expect("text"),
        Some("https://vibe.example/run".to_owned())
    );

    let mut json = super::Output::new(crate::OutputMode::Json);
    let mut stdout = Vec::new();
    for event in &events {
        json.teleport(event, &mut stdout).expect("silent");
    }
    json.finalize(&[], &mut stdout).expect("JSON");
    assert_eq!(
        String::from_utf8(stdout).expect("UTF-8"),
        "{\n  \"history\": [],\n  \"teleportUrl\": \"https://vibe.example/run\"\n}\n"
    );
}

#[test]
fn a_repository_without_a_project_is_a_teleport_error_and_anything_else_a_plain_one() {
    use vibe_app_server::client::ClientError;

    let unlinked = super::teleport_refusal(&ClientError::InvalidResponse(
        "no Vibe Code project is linked to this working directory".to_owned(),
    ));
    assert_eq!(
        unlinked.to_string(),
        "Teleport error: No Vibe Code project is linked to this repository"
    );
    let refused = super::teleport_refusal(&ClientError::InvalidResponse("git failed".to_owned()));
    assert!(
        matches!(refused, crate::CliError::Session(_)),
        "{refused:?}"
    );
}

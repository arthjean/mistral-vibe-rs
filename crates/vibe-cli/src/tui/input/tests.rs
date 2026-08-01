use super::*;

#[test]
fn editing_uses_unicode_graphemes_and_preserves_multiline_history() {
    let mut editor = PromptEditor::default();
    editor.insert("a");
    editor.insert("e\u{301}");
    editor.insert("\nβ");
    assert_eq!(editor.cursor(), 4);
    editor.move_left(false);
    editor.move_left(false);
    editor.delete_backward();
    assert_eq!(editor.text(), "a\nβ");
    let submitted = editor.submit().expect("non-empty prompt");
    assert_eq!(submitted, "a\nβ");
    editor.set_text("draft");
    editor.history_previous();
    assert_eq!(editor.text(), "a\nβ");
    editor.history_next();
    assert_eq!(editor.text(), "draft");
}

#[test]
fn external_editor_prefers_visual_parses_arguments_and_defaults_to_nano() {
    let configured =
        SystemExternalEditor::from_sources(Some("code --wait".to_owned()), Some("nvim".to_owned()));
    assert_eq!(
        configured.command_parts(),
        Ok(vec!["code".to_owned(), "--wait".to_owned()])
    );

    let fallback = SystemExternalEditor::from_sources(None, None);
    assert_eq!(fallback.command_parts(), Ok(vec!["nano".to_owned()]));

    let invalid = SystemExternalEditor::from_sources(Some("'".to_owned()), None);
    assert_eq!(
        invalid.command_parts(),
        Err("External editor command is invalid".to_owned())
    );
}

#[test]
fn huge_paste_is_rejected_without_losing_existing_input() {
    let mut editor = PromptEditor::default();
    editor.insert("keep");
    let paste = "x".repeat(MAX_PASTE_BYTES + 1);
    assert!(matches!(
        editor.paste(&paste),
        Err(InputError::PasteTooLarge { .. })
    ));
    assert_eq!(editor.text(), "keep");
}

#[test]
fn secret_submission_never_enters_prompt_history() {
    let mut editor = PromptEditor::default();
    editor.set_text("secret-api-key");
    assert_eq!(editor.take_unrecorded().as_deref(), Some("secret-api-key"));
    editor.set_text("ordinary prompt");
    assert_eq!(editor.submit().as_deref(), Some("ordinary prompt"));
    assert_eq!(editor.history, vec!["ordinary prompt"]);
}

#[test]
fn mentions_keep_display_text_and_model_resources_separate() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    fs::write(temporary.path().join("notes.txt"), "safe context").expect("text fixture");
    fs::write(temporary.path().join("image.png"), b"image").expect("image fixture");
    let prepared = prepare_submission(temporary.path(), "inspect @notes.txt and @image.png")
        .expect("mentions prepare");
    assert_eq!(prepared.turn.prompt, "inspect @notes.txt and @image.png");
    assert_eq!(prepared.turn.input.len(), 3);
    assert_eq!(prepared.metrics.len(), 2);
    let PublicContentBlock::Image { attachment } = &prepared.turn.input[2] else {
        return;
    };
    assert_eq!(attachment["mediaType"], "image/png");
    assert_eq!(
        BASE64_STANDARD
            .decode(attachment["data"].as_str().unwrap_or_default())
            .expect("canonical image data"),
        b"image"
    );
    assert_eq!(
        prepared
            .turn
            .user_display_content
            .as_ref()
            .and_then(|value| value.get("text"))
            .and_then(serde_json::Value::as_str),
        Some("inspect @notes.txt and @image.png")
    );
}

#[test]
fn pasted_workspace_images_become_quoted_mentions() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let image = temporary.path().join("image one.png");
    fs::write(&image, b"image").expect("image fixture");

    let pasted = normalize_pasted_text(temporary.path(), &format!("'{}'", image.to_string_lossy()));
    assert_eq!(pasted, "@'image one.png'");

    let prepared = prepare_submission(temporary.path(), &format!("inspect {pasted}"))
        .expect("quoted image mention prepares");
    assert_eq!(prepared.turn.input.len(), 2);
    assert_eq!(prepared.metrics[0].kind, "image");

    let text = temporary.path().join("notes one.txt");
    fs::write(&text, "context").expect("text fixture");
    assert_eq!(
        normalize_pasted_text(temporary.path(), &text.to_string_lossy()),
        text.to_string_lossy()
    );
}

#[test]
fn clipboard_images_are_marked_for_cleanup_after_their_bytes_are_embedded() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let image = temporary.path().join(".vibe-clipboard-1.png");
    fs::write(&image, b"image").expect("clipboard image");

    let prepared = prepare_submission(temporary.path(), "inspect @'.vibe-clipboard-1.png'")
        .expect("clipboard image prepares");

    assert_eq!(prepared.cleanup_paths, [image]);
    assert!(matches!(
        prepared.turn.input.get(1),
        Some(PublicContentBlock::Image { .. })
    ));
}

#[test]
fn binary_and_external_mentions_fail_without_partial_submission() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    fs::write(temporary.path().join("binary"), b"a\0b").expect("binary fixture");
    assert!(matches!(
        prepare_submission(temporary.path(), "@binary"),
        Err(InputError::BinaryMention(_))
    ));
    assert!(matches!(
        prepare_submission(temporary.path(), "@../outside"),
        Err(InputError::MentionOutsideWorkspace(_))
    ));
}

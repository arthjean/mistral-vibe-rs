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
fn paste_above_the_old_rust_boundary_is_accepted_atomically() {
    let mut editor = PromptEditor::default();
    editor.insert("keep");
    let paste = "x".repeat(256 * 1024 + 1);
    editor.paste(&paste);
    assert_eq!(editor.text().len(), "keep".len() + paste.len());
    assert!(editor.text().starts_with("keep"));
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
fn syntax_counts_follow_edits_history_and_submission() {
    let mut editor = PromptEditor::default();
    editor.set_text("alpha/@~\"'");
    assert!(editor.has_path_syntax());
    assert!(editor.has_mention_syntax());

    editor.select(5..10);
    editor.delete_backward();
    assert_eq!(editor.text(), "alpha");
    assert!(!editor.has_path_syntax());
    assert!(!editor.has_mention_syntax());

    editor.replace_history(vec!["saved/@".to_owned()]);
    assert!(editor.history_previous());
    assert!(editor.has_path_syntax());
    assert!(editor.has_mention_syntax());
    assert_eq!(editor.submit().as_deref(), Some("saved/@"));
    assert!(!editor.has_path_syntax());
    assert!(!editor.has_mention_syntax());
}

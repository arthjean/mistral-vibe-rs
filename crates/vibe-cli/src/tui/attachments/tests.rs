use std::fs;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use vibe_app_server::client::PublicContentBlock;
use vibe_core::images::MAX_IMAGE_BYTES;

use super::*;

fn draft(workspace: &Path, text: impl Into<String>, transient: &[PathBuf]) -> PromptDraft {
    let tracked = transient
        .iter()
        .cloned()
        .map(|path| (path, ImageDigest::of(b"image")))
        .collect();
    PromptDraft::with_transient_images(workspace, text, &tracked)
}

#[test]
fn only_supported_images_become_attachments() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    fs::write(temporary.path().join("notes.txt"), "safe context").expect("text fixture");
    fs::write(temporary.path().join("image.png"), b"image").expect("image fixture");
    fs::create_dir(temporary.path().join("src")).expect("directory fixture");
    fs::write(temporary.path().join("binary"), b"a\0b").expect("binary fixture");
    let prompt = draft(
        temporary.path(),
        "inspect @notes.txt @src/ @binary @missing and @image.png",
        &[],
    );
    let prepared = prepare_submission(temporary.path(), &prompt, "test-model", true)
        .expect("mentions prepare");
    assert_eq!(
        prepared.turn.prompt,
        "inspect @notes.txt @src/ @binary @missing and @image.png"
    );
    assert_eq!(prepared.turn.input.len(), 2);
    assert_eq!(prepared.mention_stats.count, 4);
    assert_eq!(
        prepared.mention_stats.context_types,
        BTreeMap::from([
            ("file".to_owned(), 2),
            ("folder".to_owned(), 1),
            ("image".to_owned(), 1),
        ])
    );
    assert_eq!(
        prepared.mention_stats.file_extensions,
        BTreeMap::from([(String::new(), 1), (".txt".to_owned(), 1)])
    );
    assert_eq!(
        prepared.turn.mention_stats,
        Some(json!({
            "count": 4,
            "contextTypes": {"file": 2, "folder": 1, "image": 1},
            "fileExtensions": {"": 1, ".txt": 1},
        }))
    );
    assert!(matches!(
        prepared.turn.input.first(),
        Some(PublicContentBlock::Text { text })
            if text == "inspect @notes.txt @src/ @binary @missing and @image.png"
    ));
    assert!(
        prepared
            .turn
            .input
            .iter()
            .all(|block| !matches!(block, PublicContentBlock::Resource { .. }))
    );
    let PublicContentBlock::Image { attachment } = &prepared.turn.input[1] else {
        panic!("second content block should be an image");
    };
    assert_eq!(attachment["alias"], "image.png");
    assert_eq!(attachment["mimeType"], "image/png");
    assert_eq!(attachment["source"]["kind"], "file");
    assert_eq!(
        attachment["source"]["path"],
        temporary
            .path()
            .join("image.png")
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(prepared.turn.user_display_content, None);
}

#[test]
fn mention_scanning_preserves_prompt_text_and_recovers_after_unmatched_quotes() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    fs::write(temporary.path().join("notes.txt"), "safe context").expect("text fixture");
    let prompt = draft(
        temporary.path(),
        "@'unterminated then @notes.txt, mail@notes.txt",
        &[],
    );

    let prepared = prepare_submission(temporary.path(), &prompt, "test-model", true)
        .expect("mention normalization succeeds");

    assert_eq!(
        prepared.turn.prompt,
        "@'unterminated then @notes.txt, mail@notes.txt"
    );
    assert_eq!(prepared.mention_stats.count, 1);
}

#[test]
fn pasted_workspace_images_become_quoted_mentions() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let image = temporary.path().join("image one.png");
    fs::write(&image, b"image").expect("image fixture");

    let pasted = normalize_pasted_text(&format!("'{}'", image.to_string_lossy()));
    assert_eq!(pasted, format!("@'{}'", image.to_string_lossy()));

    let prompt = draft(temporary.path(), format!("inspect {pasted}"), &[]);
    let prepared = prepare_submission(temporary.path(), &prompt, "test-model", true)
        .expect("quoted image mention prepares");
    assert_eq!(prepared.turn.input.len(), 2);
    assert_eq!(prepared.mention_stats.context_types["image"], 1);

    let text = temporary.path().join("notes one.txt");
    fs::write(&text, "context").expect("text fixture");
    assert_eq!(
        normalize_pasted_text(&text.to_string_lossy()),
        text.to_string_lossy()
    );

    assert_eq!(
        normalize_pasted_text(&format!(
            "drop {} here",
            image.to_string_lossy().replace(' ', "\\ ")
        )),
        format!("drop @'{}' here", image.to_string_lossy())
    );
}

#[test]
fn prompt_drafts_own_only_exact_transient_mentions() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let image = temporary.path().join("clipboard-1.png");
    fs::write(&image, b"image").expect("clipboard image");
    let image = fs::canonicalize(image).expect("canonical image");

    let unrelated = draft(
        temporary.path(),
        format!("plain {}.backup", image.to_string_lossy()),
        std::slice::from_ref(&image),
    );
    assert_eq!(unrelated.transient_image_paths().count(), 0);

    let attached = draft(
        temporary.path(),
        "inspect @'clipboard-1.png'",
        std::slice::from_ref(&image),
    );
    assert_eq!(
        attached
            .transient_image_paths()
            .cloned()
            .collect::<Vec<_>>(),
        [image]
    );
}

#[test]
fn clipboard_images_are_marked_for_cleanup_after_their_bytes_are_embedded() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let image = temporary.path().join("clipboard-1.png");
    fs::write(&image, b"image").expect("clipboard image");
    let image = fs::canonicalize(image).expect("canonical image");
    let prompt = draft(
        temporary.path(),
        "inspect @'clipboard-1.png'",
        std::slice::from_ref(&image),
    );

    let prepared = prepare_submission(temporary.path(), &prompt, "test-model", true)
        .expect("clipboard image prepares");

    assert_eq!(prepared.cleanup_paths, [image]);
    assert!(matches!(
        prepared.turn.input.get(1),
        Some(PublicContentBlock::Image { .. })
    ));
    let Some(PublicContentBlock::Image { attachment }) = prepared.turn.input.get(1) else {
        unreachable!("image content block was asserted above")
    };
    assert_eq!(attachment["source"]["kind"], "inline");
    assert_eq!(
        BASE64_STANDARD
            .decode(attachment["source"]["data"].as_str().unwrap_or_default())
            .expect("canonical image data"),
        b"image"
    );
}

#[test]
fn a_transient_image_replaced_by_another_file_before_submission_is_recoverable() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let image = temporary.path().join("clipboard-1.png");
    fs::write(&image, b"image").expect("clipboard image");
    let image = fs::canonicalize(image).expect("canonical image");
    let prompt = draft(
        temporary.path(),
        "inspect @'clipboard-1.png'",
        std::slice::from_ref(&image),
    );
    fs::write(&image, b"other").expect("replace captured image contents");

    let error = prepare_submission(temporary.path(), &prompt, "test-model", true)
        .expect_err("replacement must fail");
    assert!(matches!(&error, SubmissionError::ImageChanged { .. }));
    assert_eq!(
        error.to_string(),
        "Failed to attach image clipboard-1.png: Image changed before it could be read"
    );
    assert_eq!(prompt.text(), "inspect @'clipboard-1.png'");
}

#[test]
fn external_images_attach_but_unsupported_models_and_oversize_files_fail_atomically() {
    let temporary = tempfile::tempdir().expect("temporary workspace");
    let external = tempfile::tempdir().expect("external directory");
    let image = external.path().join("outside.png");
    fs::write(&image, b"image").expect("external image");
    let prompt = draft(
        temporary.path(),
        format!("inspect @{}", image.to_string_lossy()),
        &[],
    );

    let prepared = prepare_submission(temporary.path(), &prompt, "test-model", true)
        .expect("external image prepares");
    assert_eq!(prepared.turn.input.len(), 2);
    fs::write(&image, b"replaced").expect("replace source after preparation");
    assert_eq!(
        BASE64_STANDARD
            .decode(&prepared.provider_images[0].data)
            .expect("stable provider image"),
        b"image"
    );
    let unsupported = prepare_submission(temporary.path(), &prompt, "test-model", false)
        .expect_err("unsupported model must fail");
    assert!(matches!(
        &unsupported,
        SubmissionError::ImagesUnsupported { .. }
    ));
    assert_eq!(
        unsupported.to_string(),
        "Model `test-model` does not support images. Switch with /model or remove the attachment."
    );

    let oversized = temporary.path().join("oversized.png");
    let file = fs::File::create(&oversized).expect("oversized image");
    file.set_len(MAX_IMAGE_BYTES + 1)
        .expect("extend oversized image");
    let oversized = draft(temporary.path(), "@oversized.png", &[]);
    let error = prepare_submission(temporary.path(), &oversized, "test-model", true)
        .expect_err("oversized image must fail");
    assert!(matches!(&error, SubmissionError::ImageAttachment { .. }));
    assert_eq!(
        error.to_string(),
        format!(
            "Failed to attach image oversized.png: Image is too large: {} > {MAX_IMAGE_BYTES}",
            MAX_IMAGE_BYTES + 1
        )
    );
}

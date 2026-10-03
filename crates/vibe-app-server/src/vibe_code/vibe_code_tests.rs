use vibe_core::events::ModelMessage;

use super::*;

#[test]
fn a_missing_or_refused_project_reads_as_a_stale_saved_link() {
    for message in [
        "Project not found",
        "Vibe Code answered (status 404): unknown project",
        "Vibe Code answered (status 403): project access denied",
        "Forbidden: this project is private",
    ] {
        assert!(is_saved_project_stale(message), "{message}");
    }
    for message in [
        "Vibe Code answered (status 500): project failed",
        "Vibe Code answered (status 404): no route",
        "connection reset",
    ] {
        assert!(!is_saved_project_stale(message), "{message}");
    }
}

#[test]
fn the_prompt_falls_back_to_the_last_operator_message() {
    let messages = vec![ModelMessage::user("first"), ModelMessage::user("second")];
    assert_eq!(resolve_prompt(Some("given"), &messages), "given");
    assert_eq!(resolve_prompt(Some(""), &messages), "second");
    assert_eq!(resolve_prompt(None, &messages), "second");
    assert_eq!(resolve_prompt(None, &[]), "");
}

#[test]
fn the_context_drops_the_message_that_stands_for_the_prompt() {
    let messages = vec![ModelMessage::user("first"), ModelMessage::user("second")];
    let contents = |context: Vec<ModelMessage>| {
        context
            .iter()
            .map(|message| message.content().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(contents(context_messages(None, &messages)), ["first"]);
    assert_eq!(
        contents(context_messages(Some("given"), &messages)),
        ["first", "second"]
    );
}

#[test]
fn only_messages_with_content_carry_context() {
    assert!(carries_context(&ModelMessage::user("work")));
    assert!(!carries_context(&ModelMessage::user("")));
    assert!(!carries_context(&ModelMessage::System {
        content: "system".to_owned()
    }));
}

#[test]
fn the_summary_request_escapes_the_prompt_it_quotes() {
    let request = summary_request("a < b & c");
    assert!(request.contains("<teleported_prompt>\na &lt; b &amp; c\n</teleported_prompt>"));
}

#[test]
fn a_diff_is_compressed_then_encoded_and_an_empty_one_is_omitted() {
    use base64::Engine as _;
    assert!(compress_diff(b"").ok().flatten().is_none());
    let diff = b"diff --git a/f b/f\n+line\n".repeat(64);
    let encoded = compress_diff(&diff)
        .ok()
        .flatten()
        .expect("a small diff is accepted");
    assert_eq!(
        (encoded.format, encoded.encoding, encoded.compression),
        ("git-diff", "base64", "zstd")
    );
    let compressed = base64::engine::general_purpose::STANDARD
        .decode(encoded.content)
        .expect("base64");
    let restored = zstd::bulk::decompress(&compressed, diff.len()).expect("zstd");
    assert_eq!(restored, diff);
}

#[test]
fn a_diff_past_the_encoded_ceiling_is_refused() {
    // Random bytes do not compress, so 800 000 of them encode past the cap.
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let diff = (0..800_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect::<Vec<_>>();
    assert!(compress_diff(&diff).is_err());
}

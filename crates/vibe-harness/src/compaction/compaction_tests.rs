use super::*;

#[test]
fn a_summary_is_read_between_its_delimiters_or_taken_whole() {
    assert_eq!(
        extract_summary("<summary>\n done \n</summary>", false),
        Ok("done".to_owned())
    );
    assert_eq!(extract_summary("Done.", false), Ok("Done.".to_owned()));
    assert_eq!(extract_summary("  ", false), Err(Rejection::EmptyResponse));
    assert_eq!(
        extract_summary("half</summary>", false),
        Err(Rejection::MalformedDelimiters)
    );
    assert_eq!(
        extract_summary("<summary>open", false),
        Err(Rejection::MalformedDelimiters)
    );
    assert_eq!(
        extract_summary("<summary> </summary>", false),
        Err(Rejection::MalformedDelimiters)
    );
    assert_eq!(extract_summary("text", true), Err(Rejection::ToolCalls));
}

#[test]
fn a_replacement_keeps_what_the_person_wrote_between_the_preamble_and_the_summary() {
    let user = |text: &str, injected: bool| ConversationMessage::User {
        content: vec![serde_json::json!({"type": "text", "text": text})],
        injected,
    };
    let conversation = vec![
        user("first", false),
        ConversationMessage::Assistant {
            text: "answer".to_owned(),
            reasoning: None,
            tool_calls: Vec::new(),
        },
        user("context", true),
        user("second", false),
    ];
    let replaced = replacement(&conversation, "the summary");
    assert_eq!(replaced.len(), 4);
    assert_eq!(replaced[1], user("first", false));
    assert_eq!(replaced[2], user("second", false));
    let ConversationMessage::User { content, injected } = &replaced[3] else {
        unreachable!("the summary closes the replacement");
    };
    assert!(injected);
    assert!(
        content[0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("the summary"))
    );
}

#[test]
fn extra_instructions_are_appended_in_their_own_block() {
    assert_eq!(prompt("  "), PROMPT);
    assert!(prompt("keep the tests").ends_with(
        "<extra_compaction_instructions>\nkeep the tests\n</extra_compaction_instructions>"
    ));
}

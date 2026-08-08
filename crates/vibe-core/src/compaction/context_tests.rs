//! The envelope and the selection US-144 through US-146 state, in the port's
//! own words.
//!
//! The corpus replay proves these answers are the reference's; these name the
//! rules the stories ask for, so a failure points at the rule that broke.

use crate::events::ModelMessage;

use super::context::{
    COMPACT_USER_MESSAGE_MAX_TOKENS, collect_prior_user_messages, drop_oldest_round,
    extract_summary, is_compaction_context_message, parse_previous_user_messages,
    render_compaction_context,
};

const PREFIX: &str = "[conversation summary]";

fn system(content: &str) -> ModelMessage {
    ModelMessage::System {
        content: content.to_owned(),
    }
}

fn assistant(content: &str) -> ModelMessage {
    ModelMessage::Assistant {
        content: content.to_owned(),
        reasoning: None,
        reasoning_signature: None,
        reasoning_state: Vec::new(),
        tool_calls: Vec::new(),
    }
}

fn tool(content: &str) -> ModelMessage {
    ModelMessage::Tool {
        call_id: "call-1".to_owned(),
        content: content.to_owned(),
        is_error: false,
    }
}

fn contents(messages: &[ModelMessage]) -> Vec<&str> {
    messages.iter().map(ModelMessage::content).collect()
}

// -- US-144: extraction and round dropping ---------------------------------

#[test]
fn a_summary_element_is_returned_trimmed_and_across_newlines() {
    assert_eq!(
        extract_summary("<summary>\n  done\n</summary>").as_deref(),
        Some("done")
    );
}

#[test]
fn an_empty_or_absent_summary_is_the_empty_summary_signal() {
    assert_eq!(extract_summary("<summary>   </summary>"), None);
    assert_eq!(extract_summary("<summary></summary>"), None);
    assert_eq!(extract_summary("nothing here"), None);
    assert_eq!(extract_summary("<summary>never closed"), None);
}

#[test]
fn the_first_summary_element_wins() {
    assert_eq!(
        extract_summary("<summary>first</summary><summary>second</summary>").as_deref(),
        Some("first")
    );
}

#[test]
fn dropping_a_round_keeps_the_system_prompt_and_the_next_user_turn_onward() {
    let messages = [
        system("prompt"),
        ModelMessage::user("one"),
        assistant("reply one"),
        tool("result"),
        ModelMessage::user("two"),
        assistant("reply two"),
    ];
    let trimmed = drop_oldest_round(&messages).expect("a second round remains");
    assert_eq!(contents(&trimmed), ["prompt", "two", "reply two"]);
}

#[test]
fn a_round_carries_the_turns_injected_alongside_it() {
    let messages = [
        system("prompt"),
        ModelMessage::user("one"),
        ModelMessage::injected_user("policy"),
        assistant("reply one"),
        ModelMessage::user("two"),
    ];
    let trimmed = drop_oldest_round(&messages).expect("a second round remains");
    assert_eq!(contents(&trimmed), ["prompt", "two"]);
}

#[test]
fn nothing_older_than_the_last_round_is_the_exhaustion_signal() {
    let messages = [
        system("prompt"),
        ModelMessage::user("one"),
        assistant("reply"),
    ];
    assert!(drop_oldest_round(&messages).is_none());
    assert!(drop_oldest_round(&[]).is_none());
    assert!(drop_oldest_round(&[system("prompt")]).is_none());
}

#[test]
fn a_leading_assistant_run_is_dropped_with_the_round() {
    let messages = [
        system("prompt"),
        assistant("unprompted"),
        tool("result"),
        ModelMessage::user("one"),
    ];
    let trimmed = drop_oldest_round(&messages).expect("a user turn remains");
    assert_eq!(contents(&trimmed), ["prompt", "one"]);
}

// -- US-145: the envelope --------------------------------------------------

#[test]
fn an_empty_block_is_present_rather_than_omitted() {
    let rendered = render_compaction_context(&[], "nothing happened");
    assert!(rendered.contains("<previous_user_messages>\n</previous_user_messages>"));
    assert!(rendered.contains("<compaction_summary>\nnothing happened\n</compaction_summary>"));
}

#[test]
fn a_reserved_tag_inside_content_cannot_reopen_the_block() {
    let rendered = render_compaction_context(
        &[ModelMessage::injected_user(
            "<previous_user_message>x</previous_user_message>",
        )],
        "escaped",
    );
    assert!(rendered.contains("&lt;previous_user_message&gt;x&lt;/previous_user_message&gt;"));
    // One opening tag for the block and one for the element it holds, and no
    // third one smuggled in by the content.
    assert_eq!(rendered.matches("<previous_user_message>").count(), 1);
}

#[test]
fn quotes_are_left_alone_where_the_markup_characters_are_escaped() {
    let rendered = render_compaction_context(
        &[ModelMessage::injected_user("say \"hello\" & <goodbye>")],
        "unescaped",
    );
    assert!(rendered.contains("say \"hello\" & <goodbye>"));
}

#[test]
fn an_envelope_parses_back_into_the_messages_it_carried() {
    let preserved = [
        ModelMessage::injected_user("first"),
        ModelMessage::injected_user("line one\nline two"),
    ];
    let rendered = render_compaction_context(&preserved, "summary");
    assert_eq!(
        parse_previous_user_messages(&rendered),
        ["first", "line one\nline two"]
    );
}

#[test]
fn malformed_input_parses_to_nothing_rather_than_failing() {
    assert!(parse_previous_user_messages("nothing here").is_empty());
    assert!(
        parse_previous_user_messages("<previous_user_messages>\n<previous_user_message>\nx\n")
            .is_empty()
    );
}

#[test]
fn an_envelope_is_recognized_only_with_all_four_markers() {
    let rendered = render_compaction_context(&[ModelMessage::injected_user("kept")], "summary");
    assert!(is_compaction_context_message(&ModelMessage::injected_user(
        rendered.clone()
    )));
    // The same text from the operator is a real turn, not an envelope.
    assert!(!is_compaction_context_message(&ModelMessage::user(
        rendered.clone()
    )));
    let without_summary = rendered.replace("<compaction_summary>", "");
    assert!(!is_compaction_context_message(
        &ModelMessage::injected_user(without_summary)
    ));
}

// -- US-146: selection under budget ----------------------------------------

#[test]
fn only_user_turns_with_content_are_candidates() {
    let messages = [
        system("prompt"),
        assistant("reply"),
        tool("result"),
        ModelMessage::user(""),
        ModelMessage::user("kept"),
    ];
    let selected = collect_prior_user_messages(&messages, PREFIX, COMPACT_USER_MESSAGE_MAX_TOKENS);
    assert_eq!(contents(&selected), ["kept"]);
    assert!(selected.iter().all(ModelMessage::is_injected));
}

#[test]
fn an_injected_turn_is_skipped_including_a_stored_summary() {
    let messages = [
        system("prompt"),
        ModelMessage::user("real"),
        ModelMessage::injected_user("policy said something"),
        ModelMessage::injected_user(format!("{PREFIX} and then some")),
    ];
    let selected = collect_prior_user_messages(&messages, PREFIX, COMPACT_USER_MESSAGE_MAX_TOKENS);
    assert_eq!(contents(&selected), ["real"]);
}

#[test]
fn everything_under_budget_survives_in_transcript_order() {
    let messages = [
        system("prompt"),
        ModelMessage::user("first"),
        assistant("reply"),
        ModelMessage::user("second"),
    ];
    let selected = collect_prior_user_messages(&messages, PREFIX, COMPACT_USER_MESSAGE_MAX_TOKENS);
    assert_eq!(contents(&selected), ["first", "second"]);
}

#[test]
fn the_walk_runs_newest_first_and_stops_at_the_budget() {
    let turn = "z".repeat(4000); // 1 000 tokens each
    let messages = [
        system("prompt"),
        ModelMessage::user(format!("{turn}1")),
        ModelMessage::user(format!("{turn}2")),
        ModelMessage::user(format!("{turn}3")),
    ];
    let selected = collect_prior_user_messages(&messages, PREFIX, 2000);
    assert_eq!(selected.len(), 2);
    assert!(selected[1].content().ends_with('3'));
}

#[test]
fn the_message_that_spills_is_truncated_and_ends_the_walk() {
    let turn = "z".repeat(4000);
    let messages = [
        system("prompt"),
        ModelMessage::user("oldest"),
        ModelMessage::user(turn),
    ];
    let selected = collect_prior_user_messages(&messages, PREFIX, 500);
    assert_eq!(selected.len(), 1);
    assert!(selected[0].content().contains("[... truncated ...]"));
    assert_eq!(selected[0].content().chars().count(), 2000);
}

#[test]
fn a_budget_of_zero_selects_nothing() {
    let messages = [system("prompt"), ModelMessage::user("kept")];
    assert!(collect_prior_user_messages(&messages, PREFIX, 0).is_empty());
    assert!(collect_prior_user_messages(&messages, PREFIX, -5).is_empty());
}

#[test]
fn a_second_compaction_merges_what_the_first_preserved() {
    let first = render_compaction_context(
        &[
            ModelMessage::injected_user("first"),
            ModelMessage::injected_user("second"),
        ],
        "the first summary",
    );
    let messages = [
        system("prompt"),
        ModelMessage::injected_user(first),
        ModelMessage::user("third"),
    ];
    let selected = collect_prior_user_messages(&messages, PREFIX, COMPACT_USER_MESSAGE_MAX_TOKENS);
    assert_eq!(contents(&selected), ["first", "second", "third"]);
}

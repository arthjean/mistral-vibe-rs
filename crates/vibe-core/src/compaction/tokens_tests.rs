//! The arithmetic US-143 states, in the port's own words.
//!
//! The corpus replay is what proves these answers are the reference's; these
//! name the properties the story asks for, so a failure points at the rule that
//! broke rather than at a scenario index.

use super::tokens::{approx_token_count, truncate_middle_to_tokens};

const MARKER: &str = "\n\n[... truncated ...]\n\n";

#[test]
fn a_count_is_the_code_point_count_rounded_up() {
    assert_eq!(approx_token_count(""), 0);
    assert_eq!(approx_token_count("a"), 1);
    assert_eq!(approx_token_count("abcd"), 1);
    assert_eq!(approx_token_count("abcde"), 2);
}

#[test]
fn a_count_measures_code_points_and_never_utf8_bytes() {
    // Five code points, fifteen UTF-8 bytes: a byte-counting port answers 4.
    let text = "héllo";
    assert_eq!(text.len(), 6);
    assert_eq!(approx_token_count(text), 2);

    // Four astral-plane characters, sixteen bytes.
    let emoji = "🙂🙃🙂🙃";
    assert_eq!(emoji.len(), 16);
    assert_eq!(approx_token_count(emoji), 1);
}

#[test]
fn a_budget_of_zero_or_less_truncates_to_nothing() {
    assert_eq!(truncate_middle_to_tokens("abcdef", 0), "");
    assert_eq!(truncate_middle_to_tokens("abcdef", -1), "");
    assert_eq!(truncate_middle_to_tokens("abcdef", i64::MIN), "");
}

#[test]
fn a_string_that_fits_is_returned_unchanged() {
    assert_eq!(truncate_middle_to_tokens("abcd", 1), "abcd");
    assert_eq!(truncate_middle_to_tokens("abcdefgh", 2), "abcdefgh");
}

#[test]
fn an_odd_allowance_gives_the_remainder_to_the_tail() {
    // 8 tokens is 32 characters, the marker takes 23, so 9 remain: 4 head, 5
    // tail.
    let truncated = truncate_middle_to_tokens(&"abcdefghij".repeat(6), 8);
    assert_eq!(truncated, format!("abcd{MARKER}fghij"));
}

#[test]
fn an_allowance_at_most_the_marker_keeps_the_head_alone() {
    // 5 tokens is 20 characters, below the marker's 23, so nothing is marked.
    let truncated = truncate_middle_to_tokens(&"x".repeat(40), 5);
    assert_eq!(truncated, "x".repeat(20));
    assert!(!truncated.contains(MARKER));
}

#[test]
fn a_multi_byte_character_on_a_boundary_never_splits() {
    let truncated = truncate_middle_to_tokens(&"🙂🙃".repeat(40), 7);
    assert!(truncated.contains(MARKER));
    // Every kept character survived whole, which is what `String` cannot
    // represent otherwise.
    assert!(
        truncated
            .replace(MARKER, "")
            .chars()
            .all(|character| character == '🙂' || character == '🙃')
    );
}

//! The token arithmetic every compaction budget decision rests on.
//!
//! Both functions are approximations the reference applies uniformly, so
//! reproducing them exactly matters more than making them better: a count that
//! disagrees by one selects a different set of messages to preserve, and a
//! truncation that splits a budget the other way writes a different envelope.
//!
//! Two traps live here, both about what a character is. The reference measures
//! a Python `str`, whose length is its **code point** count, and slices it by
//! code point as well. Measuring UTF-8 bytes instead diverges silently on any
//! transcript carrying a non-ASCII character, and the divergence is a budget
//! error, so it changes which messages survive a compaction.
//!
//! Reference: `vibe/core/utils/tokens.py` at the pinned commit.

/// How many characters the reference charges to one token.
const APPROX_CHARS_PER_TOKEN: usize = 4;

/// What the reference writes where the middle of a message was removed.
const TRUNCATION_MARKER: &str = "\n\n[... truncated ...]\n\n";

/// The reference's approximate token count: the code-point count divided by
/// four, rounded up.
#[must_use]
pub fn approx_token_count(text: &str) -> usize {
    text.chars().count().div_ceil(APPROX_CHARS_PER_TOKEN)
}

/// `text` shrunk to fit `max_tokens` by dropping its middle.
///
/// The head and the tail are kept because intent and constraints usually sit at
/// the ends of a user message. An odd allowance goes to the tail: the reference
/// takes the floor of half for the head and gives the remainder to the tail.
///
/// `max_tokens` is signed because the reference guards against a budget of zero
/// **or less** and answers the empty string for both, and a caller that already
/// spent its budget hands the remainder straight through.
#[must_use]
pub fn truncate_middle_to_tokens(text: &str, max_tokens: i64) -> String {
    if max_tokens <= 0 {
        return String::new();
    }
    let max_chars = usize::try_from(max_tokens).unwrap_or(usize::MAX);
    let max_chars = max_chars.saturating_mul(APPROX_CHARS_PER_TOKEN);
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let marker_chars = TRUNCATION_MARKER.chars().count();
    // No room for the marker and something on either side of it: the reference
    // keeps the head alone and says nothing about what it dropped.
    let Some(available) = max_chars
        .checked_sub(marker_chars)
        .filter(|available| *available > 0)
    else {
        return head(text, max_chars);
    };
    let kept = available / 2;
    let mut truncated = head(text, kept);
    truncated.push_str(TRUNCATION_MARKER);
    truncated.push_str(&tail(text, available - kept));
    truncated
}

/// The first `characters` code points of `text`, never splitting one.
fn head(text: &str, characters: usize) -> String {
    text.chars().take(characters).collect()
}

/// The last `characters` code points of `text`, never splitting one.
fn tail(text: &str, characters: usize) -> String {
    let total = text.chars().count();
    text.chars()
        .skip(total.saturating_sub(characters))
        .collect()
}

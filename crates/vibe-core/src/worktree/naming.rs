//! Names for worktrees nobody named.
//!
//! A bare `--worktree` and a session asking for a worktree of its own both
//! leave the name to Vibe. It comes from the naming model's suggestion, else
//! from the prompt, else from a random slug, and whichever it is goes through
//! the same slugifier, so a name that reaches git is portable however it was
//! produced (`vibe/core/git/worktree/naming.py`,
//! `vibe/core/git/worktree/repository.py:1103-1119`).

use super::slug_table::SLUG_PIECES;

/// The longest name a slug produces, suffix included
/// (`vibe/core/git/worktree/naming.py:6`).
pub const MAX_WORKTREE_NAME_LENGTH: usize = 40;

/// How many words of the text a name keeps.
const MAX_WORKTREE_NAME_WORDS: usize = 6;

/// Trailing filler that reads as noise in a folder name. English only: text in
/// another language keeps the word-boundary cut alone.
const STOP_WORDS: [&str; 15] = [
    "a", "an", "and", "at", "for", "in", "is", "it", "me", "my", "of", "on", "the", "to", "with",
];

/// A worktree name from free text: its first six words, slugified, cut to the
/// length limit on a word boundary, trailing filler dropped.
#[must_use]
pub fn worktree_name_from_text(text: &str) -> String {
    let slug = slugify(text);
    let words = slug
        .split('-')
        .take(MAX_WORKTREE_NAME_WORDS)
        .collect::<Vec<_>>()
        .join("-");
    trim(&words, 0)
}

/// `name` with a numeric suffix, the name cut first so the whole still fits.
#[must_use]
pub fn worktree_name_with_suffix(name: &str, suffix: usize) -> String {
    let suffix_text = format!("-{suffix}");
    format!("{}{suffix_text}", trim(name, suffix_text.len()))
}

/// Lowercase ASCII letters and digits, every other run one hyphen, no hyphen
/// at either end.
///
/// Each character contributes what its compatibility decomposition lowercases
/// to, which is how an accented letter keeps its base and a full-width digit
/// becomes a digit. A decomposition's combining marks are separators, as the
/// reference's pattern makes them.
pub(crate) fn slugify(value: &str) -> String {
    let mut raw = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii() {
            let lowered = character.to_ascii_lowercase();
            raw.push(if lowered.is_ascii_alphanumeric() {
                lowered
            } else {
                '-'
            });
            continue;
        }
        match SLUG_PIECES.binary_search_by_key(&u32::from(character), |(code, _)| *code) {
            Ok(index) => raw.push_str(SLUG_PIECES[index].1),
            Err(_) => raw.push('-'),
        }
    }
    let mut collapsed = String::with_capacity(raw.len());
    for character in raw.chars() {
        if character == '-' && collapsed.ends_with('-') {
            continue;
        }
        collapsed.push(character);
    }
    collapsed.trim_matches('-').to_owned()
}

fn trim(value: &str, reserved: usize) -> String {
    let max_length = MAX_WORKTREE_NAME_LENGTH.saturating_sub(reserved);
    let truncated = if value.len() <= max_length {
        value
    } else {
        cut_on_word_boundary(value, max_length)
    };
    let words = truncated
        .split('-')
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    without_trailing_stop_words(&words).join("-")
}

/// The first `max_length` characters, backed off to the last hyphen unless the
/// cut already falls on one. A slug is ASCII, so bytes are characters.
fn cut_on_word_boundary(value: &str, max_length: usize) -> &str {
    let cut = &value[..max_length];
    if value.as_bytes()[max_length] == b'-' {
        return cut;
    }
    match cut.rfind('-') {
        Some(boundary) => &cut[..boundary],
        None => cut,
    }
}

fn without_trailing_stop_words<'a>(words: &[&'a str]) -> Vec<&'a str> {
    let mut end = words.len();
    while end > 1 && STOP_WORDS.contains(&words[end - 1]) {
        end -= 1;
    }
    words[..end].to_vec()
}

/// Two distinct adjectives and a noun, for a worktree with nothing to be named
/// after. The reference draws the same shape (`vibe/core/utils/slug.py:110-113`);
/// the words are this port's own.
#[must_use]
pub fn random_slug() -> String {
    const ADJECTIVES: [&str; 32] = [
        "amber", "brisk", "calm", "clever", "crisp", "dapper", "eager", "fleet", "gentle", "glad",
        "golden", "hardy", "keen", "lively", "lucid", "mellow", "nimble", "noble", "placid",
        "plucky", "quiet", "rapid", "rustic", "serene", "silver", "snug", "steady", "sunny",
        "swift", "tidy", "vivid", "witty",
    ];
    const NOUNS: [&str; 32] = [
        "badger", "beacon", "birch", "brook", "canyon", "cedar", "comet", "coral", "delta",
        "ember", "falcon", "fern", "fjord", "glacier", "harbor", "heron", "lagoon", "lantern",
        "maple", "meadow", "otter", "pebble", "quartz", "raven", "ridge", "sparrow", "summit",
        "thistle", "tundra", "valley", "walrus", "zephyr",
    ];
    let mut bytes = [0_u8; 3];
    // The words only have to differ between calls, so a failed draw degrades
    // to fixed indices rather than failing the preparation that asked.
    let _ = getrandom::fill(&mut bytes);
    let first = usize::from(bytes[0]) % ADJECTIVES.len();
    let mut second = usize::from(bytes[1]) % (ADJECTIVES.len() - 1);
    if second >= first {
        second += 1;
    }
    let noun = usize::from(bytes[2]) % NOUNS.len();
    format!(
        "{}-{}-{}",
        ADJECTIVES[first], ADJECTIVES[second], NOUNS[noun]
    )
}

//! The `str` methods whose answers differ from their nearest Rust equivalent.

use std::sync::LazyLock;

use regex::Regex;

/// `str.isspace` for one character, which is what `str.strip()` with no
/// argument removes.
///
/// Rust's `char::is_whitespace` reads the Unicode `White_Space` property,
/// which leaves out the four ASCII information separators (`U+001C` to
/// `U+001F`) Python counts as whitespace. `int()` and `float()` strip
/// `White_Space` only, so they trim with `str::trim` rather than this.
#[must_use]
pub fn isspace(character: char) -> bool {
    character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
}

/// `str.strip()` with no argument.
#[must_use]
pub fn strip(text: &str) -> &str {
    text.trim_matches(isspace)
}

/// `text[:limit].rstrip() + "…"` when `text` is longer than `limit`
/// characters, and `text` itself otherwise: how the reference cuts a title
/// or a preview down to length.
#[must_use]
pub fn clip(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", text[..end].trim_end_matches(isspace)),
        None => text.to_owned(),
    }
}

/// Whether `character` ends a line for `str.splitlines`.
#[must_use]
pub const fn is_line_boundary(character: char) -> bool {
    matches!(
        character,
        '\n' | '\r'
            | '\u{b}'      // line tabulation
            | '\u{c}'      // form feed
            | '\u{1c}'     // file separator
            | '\u{1d}'     // group separator
            | '\u{1e}'     // record separator
            | '\u{85}'     // next line
            | '\u{2028}'   // line separator
            | '\u{2029}' // paragraph separator
    )
}

/// `str.splitlines(keepends)`.
///
/// `\r\n` is one boundary, an empty text has no lines, and a final line with
/// no separator is still a line. With `keepends`, joining the lines gives the
/// text back exactly.
#[must_use]
pub fn splitlines(text: &str, keepends: bool) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut characters = text.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        if !is_line_boundary(character) {
            continue;
        }
        let mut end = index + character.len_utf8();
        if character == '\r' && characters.peek().is_some_and(|(_, next)| *next == '\n') {
            characters.next();
            end += '\n'.len_utf8();
        }
        lines.push(&text[start..if keepends { end } else { index }]);
        start = end;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// The characters `str.isprintable` refuses besides ASCII: the Unicode
/// `Other` and `Separator` categories, which `regex` carries the tables for.
#[expect(
    clippy::expect_used,
    reason = "the pattern is a compile-time constant that compiles"
)]
static NON_PRINTABLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\p{C}\p{Z}]").expect("the non-printable pattern compiles"));

/// `str.isprintable` for one character: nothing in the Unicode `Other` or
/// `Separator` categories, the ASCII space excepted.
#[must_use]
pub fn isprintable(character: char) -> bool {
    if character.is_ascii() {
        return matches!(character, ' '..='~');
    }
    !NON_PRINTABLE.is_match(character.encode_utf8(&mut [0; 4]))
}

/// `str.title()`: a cased character that follows another cased character is
/// lowercased, and one that follows anything else is uppercased.
///
/// Rust has no titlecase mapping, so the few characters whose titlecase is
/// not their uppercase (the `ǆ` digraphs, `ß`, the ligatures) take their
/// uppercase here.
#[must_use]
pub fn title(text: &str) -> String {
    let mut titled = String::with_capacity(text.len());
    let mut previous_cased = false;
    for character in text.chars() {
        let cased = character.is_uppercase() || character.is_lowercase();
        if cased && previous_cased {
            titled.extend(character.to_lowercase());
        } else if cased {
            titled.extend(character.to_uppercase());
        } else {
            titled.push(character);
        }
        previous_cased = cased;
    }
    titled
}

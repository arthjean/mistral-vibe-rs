//! A file's bytes at one point in the log, and the lines a region addresses.
//!
//! The reference splits decoded text with `str.splitlines(keepends=True)`
//! (`vibe/core/checkpoints/history.py:25`), which is not `split('\n')`: Python
//! ends a line on eight further characters, and `decode_safe`
//! (`vibe/utils/io.py:103`) has already collapsed `\r\n` and a lone `\r` to
//! `\n` before the split ever sees them. A file carrying a form feed therefore
//! holds more lines than a naive split reports, and every line index in a
//! region, an anchor and a dependency edge would shift by that difference. So
//! the boundary set is reproduced in full here rather than approximated, and
//! the parity corpus records what the reference answered for each of them.

use crate::workspace::text_file::decode;

/// One file's bytes at a point in the log, or its absence.
///
/// Absence is a state rather than an error: a file the agent deleted and a file
/// it has not created yet are both reviewable, and a revert has to be able to
/// restore either side. Mirrors the reference `FileState`
/// (`vibe/core/checkpoints/models.py:22`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileState {
    data: Option<Vec<u8>>,
}

impl FileState {
    /// The state of a path that does not exist.
    #[must_use]
    pub const fn absent() -> Self {
        Self { data: None }
    }

    /// The state holding `bytes`, exactly as they sit on disk.
    #[must_use]
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            data: Some(bytes.into()),
        }
    }

    /// The state holding `text` encoded as UTF-8.
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        Self::from_bytes(text.as_bytes())
    }

    /// The bytes, or [`None`] when the path does not exist.
    #[must_use]
    pub fn data(&self) -> Option<&[u8]> {
        self.data.as_deref()
    }

    /// Whether the path exists at this point in the log.
    #[must_use]
    pub const fn exists(&self) -> bool {
        self.data.is_some()
    }

    /// Whether the bytes carry a NUL, which is what the reference calls binary
    /// and what makes a change reviewable only as a whole file.
    #[must_use]
    pub fn is_binary(&self) -> bool {
        self.data
            .as_ref()
            .is_some_and(|bytes| bytes.contains(&b'\0'))
    }
}

/// The keepends lines of `state`, or [`None`] when it is binary.
///
/// An absent file answers an empty list rather than [`None`], because it has no
/// lines and is not binary; that distinction is what lets an opaque change name
/// `missing` and `binary_or_undecodable` apart. Mirrors `_decode_lines`
/// (`vibe/core/checkpoints/history.py:25`).
#[must_use]
pub fn decode_lines(state: &FileState) -> Option<Vec<String>> {
    let Some(bytes) = state.data() else {
        return Some(Vec::new());
    };
    if state.is_binary() {
        return None;
    }
    Some(split_lines(&decode(bytes).text))
}

/// Whether `character` ends a line for Python's `str.splitlines`.
///
/// `\r` is in the set even though `decode` has already replaced it, so this
/// function answers for any text rather than only for text that came through
/// the decoder.
const fn is_line_boundary(character: char) -> bool {
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

/// `text` split into lines that keep their separator, the way Python's
/// `str.splitlines(keepends=True)` splits.
///
/// An empty input answers an empty list, not one empty line, and a final line
/// with no separator is still a line. `\r\n` counts as one boundary, so it is
/// never split into two lines. Joining the result reproduces `text` exactly,
/// which is what lets a reconstruction splice line spans and re-encode the
/// concatenation.
#[must_use]
pub fn split_lines(text: &str) -> Vec<String> {
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
        lines.push(text[start..end].to_owned());
        start = end;
    }
    if start < text.len() {
        lines.push(text[start..].to_owned());
    }
    lines
}

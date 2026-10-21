//! A file's bytes at one point in the log, and the lines a region addresses.
//!
//! The reference splits decoded text with `str.splitlines(keepends=True)`
//! (`vibe/core/checkpoints/history.py:25`), which is not `split('\n')`: Python
//! ends a line on eight further characters, and `decode_safe`
//! (`vibe/utils/io.py:103`) has already collapsed `\r\n` and a lone `\r` to
//! `\n` before the split ever sees them. A file carrying a form feed therefore
//! holds more lines than a naive split reports, and every line index in a
//! region, an anchor and a dependency edge would shift by that difference. So
//! the split is [`pycompat::splitlines`] rather than an approximation, and the
//! parity corpus records what the reference answered for each boundary.

use std::sync::Arc;

use crate::pycompat;
use crate::workspace::text_file::decode;

/// One file's bytes at a point in the log, or its absence.
///
/// Absence is a state rather than an error: a file the agent deleted and a file
/// it has not created yet are both reviewable, and a revert has to be able to
/// restore either side. Mirrors the reference `FileState`
/// (`vibe/core/checkpoints/models.py:22`).
///
/// The bytes sit behind an [`Arc`] because the log holds a state per side of
/// every edit and a reconstruction hands the same state along a chain of
/// splices. Cloning is what the log does constantly, so cloning is what has to
/// be cheap; equality still compares the bytes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileState {
    data: Option<Arc<[u8]>>,
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
            data: Some(Arc::from(bytes.into().into_boxed_slice())),
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
    Some(
        pycompat::splitlines(&decode(bytes).text, true)
            .into_iter()
            .map(str::to_owned)
            .collect(),
    )
}

//! What the file surface answers: a read, a listing entry, a mutation, and
//! the three results the file tools publish.

use serde::{Deserialize, Serialize};

use super::EditOccurrence;
use crate::tools::reference_text;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileRead {
    pub path: String,
    pub content: String,
    pub numbered_content: String,
    pub start_line: usize,
    pub end_line: usize,
    /// Byte length of `content`, which is the selected line range rather than
    /// the whole file.
    pub content_bytes: usize,
    /// Lines the file holds, which is what tells an empty selection caused by
    /// an out-of-range offset apart from an empty file.
    pub total_lines: usize,
    pub truncated: bool,
}

/// What `read_file` publishes, matching reference `ReadFileResult`.
///
/// The field names and their order are the contract: the reference agent loop
/// renders the result one `field: value` line at a time, so a renamed or
/// reordered field changes what the model reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadFileResult {
    pub file_path: String,
    pub content: String,
    pub num_lines: usize,
    pub start_line: usize,
    pub requested_offset: Option<usize>,
    pub requested_limit: usize,
    /// [`None`] when the read stopped at a budget, because the file's real
    /// length is unknown without reading the rest of it.
    pub total_lines: Option<usize>,
    pub was_truncated: bool,
}

impl ReadFileResult {
    /// The text the model reads back.
    #[must_use]
    pub fn model_text(&self) -> String {
        reference_text::joined(&[
            ("file_path", self.file_path.clone()),
            ("content", self.content.clone()),
            ("num_lines", self.num_lines.to_string()),
            ("start_line", self.start_line.to_string()),
            (
                "requested_offset",
                reference_text::optional(self.requested_offset),
            ),
            ("requested_limit", self.requested_limit.to_string()),
            ("total_lines", reference_text::optional(self.total_lines)),
            (
                "was_truncated",
                reference_text::boolean(self.was_truncated).to_owned(),
            ),
        ])
    }
}

/// What `write_file` publishes, matching reference `WriteFileResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteFileResult {
    pub file_path: String,
    pub bytes_written: usize,
    pub content: String,
}

impl WriteFileResult {
    #[must_use]
    pub fn model_text(&self) -> String {
        reference_text::joined(&[
            ("file_path", self.file_path.clone()),
            ("bytes_written", self.bytes_written.to_string()),
            ("content", self.content.clone()),
        ])
    }
}

/// What `edit` publishes, matching reference `EditResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditResult {
    pub file: String,
    pub message: String,
    pub old_string: String,
    pub new_string: String,
}

impl EditResult {
    #[must_use]
    pub fn model_text(&self) -> String {
        reference_text::joined(&[
            ("file", self.file.clone()),
            ("message", self.message.clone()),
            ("old_string", self.old_string.clone()),
            ("new_string", self.new_string.clone()),
        ])
    }
}

/// One bounded read, the way reference `read_lines_safe` bounds one.
///
/// The line budget and the byte budget both stop the read, and either one
/// leaves `total_lines` unknown: the reference only reports a total when it
/// reached the end of the file, because anything else would be a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedRead {
    pub lines: Vec<String>,
    pub total_lines: Option<usize>,
    pub was_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub path: String,
    pub is_directory: bool,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MutationResult {
    pub path: String,
    pub bytes_written: usize,
    pub files_changed: usize,
    pub diff: String,
    /// The replacements one edit performed, in the order they appear in the
    /// file as it stood before the edit. A mutation that replaced nothing,
    /// like a fresh write, carries none.
    #[serde(default)]
    pub occurrences: Vec<EditOccurrence>,
}

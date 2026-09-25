//! Server-sent event framing.
//!
//! Reference `iter_sse_lines` (`vibe/core/utils/sse.py`) splits a body on CRLF,
//! LF and CR only: a JSON string may carry U+2028 or U+0085 unescaped, and a
//! splitter that also breaks there would cut a payload in half. A CR that ends
//! a network chunk is held back, since it may be the first half of a CRLF.
//!
//! [`data_line`] is the generic backend's reading of one line
//! (`GenericBackend._send_streaming_request`): blank lines and comments are
//! skipped, a line without `": "` is refused, a key other than `data` is
//! ignored, and `[DONE]` ends the stream.

use serde_json::Value;

/// Splits a byte stream into lines as it arrives.
#[derive(Debug, Default)]
pub struct LineSplitter {
    pending: Vec<u8>,
}

impl LineSplitter {
    /// The complete lines `bytes` finishes, in order.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let held_cr = self.pending.last() == Some(&b'\r');
        if held_cr {
            self.pending.pop();
        }
        let normalized = normalize_newlines(&self.pending);
        let mut pieces: Vec<&[u8]> = normalized.split(|byte| *byte == b'\n').collect();
        let rest = pieces.pop().unwrap_or_default().to_vec();
        let lines = pieces
            .into_iter()
            .map(|line| String::from_utf8_lossy(line).into_owned())
            .collect();
        self.pending = rest;
        if held_cr {
            self.pending.push(b'\r');
        }
        lines
    }

    /// The unterminated last line, if the body ended without a newline.
    #[must_use]
    pub fn finish(self) -> Option<String> {
        let mut rest = self.pending;
        if rest.is_empty() {
            return None;
        }
        if rest.last() == Some(&b'\r') {
            rest.pop();
        }
        Some(String::from_utf8_lossy(&rest).into_owned())
    }
}

fn normalize_newlines(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' => {
                out.push(b'\n');
                if bytes.get(index + 1) == Some(&b'\n') {
                    index += 1;
                }
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    out
}

/// What one line of an event stream means to the generic backend.
#[derive(Debug, Clone, PartialEq)]
pub enum DataLine {
    /// Nothing to read: a blank line, a comment, or a field other than `data`.
    Skip,
    /// The `[DONE]` sentinel: the stream is over.
    Done,
    Event(Value),
}

/// Why a line could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LineError {
    #[error("an event stream line is not formatted as `key: value`")]
    Format,
    #[error("an event stream line carries malformed JSON")]
    Json,
}

/// Reads one line the generic backend received.
///
/// # Errors
///
/// A line that is neither blank, a comment, nor `key: value`, and a `data`
/// value that is not JSON.
pub fn data_line(line: &str) -> Result<DataLine, LineError> {
    if python_strip(line).is_empty() || line.starts_with(':') {
        return Ok(DataLine::Skip);
    }
    if !line.contains(": ") {
        return Err(LineError::Format);
    }
    let Some((key, after)) = line.split_once(':') else {
        return Err(LineError::Format);
    };
    // The value starts two characters after the colon whatever the second
    // one is, as the reference slices it.
    let mut rest = after.chars();
    rest.next();
    let value = rest.as_str();
    if key != "data" {
        return Ok(DataLine::Skip);
    }
    let value = python_strip(value);
    if value == "[DONE]" {
        return Ok(DataLine::Done);
    }
    serde_json::from_str(value)
        .map(DataLine::Event)
        .map_err(|_| LineError::Json)
}

/// `str.strip()` with no argument: Python's whitespace also counts the four
/// information separators U+001C to U+001F, which Rust's does not.
#[must_use]
pub fn python_strip(text: &str) -> &str {
    text.trim_matches(is_python_space)
}

#[must_use]
pub fn is_python_space(character: char) -> bool {
    character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
}

//! Turning captured console bytes into the text a model reads.
//!
//! Two problems live here and nowhere else in the family. A Windows shell emits
//! UTF-16 often enough that reading its output as UTF-8 produces a string of
//! interleaved NULs, so an unmarked stream that is plainly UTF-16 is decoded as
//! such. And a window bounded by bytes can cut a character in half, at its end
//! when the log is still growing and at its start when the window was
//! positioned by subtracting a budget from the file size; both ends are trimmed
//! so a poll never emits a replacement character the next poll cannot take
//! back.
//!
//! Everything here is a pure function of bytes, which is what lets a decoding
//! case be proven without a process.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::Path;

use crate::process::{ProcessChunk, ProcessStream};
use crate::tools::ToolError;

/// The bytes one stream produced, decoded and bounded by `limit`.
pub(super) fn render_stream(
    chunks: &[ProcessChunk],
    stream: ProcessStream,
    limit: usize,
) -> (String, bool) {
    let mut bytes = Vec::new();
    for chunk in chunks.iter().filter(|chunk| chunk.stream == stream) {
        bytes.extend_from_slice(&chunk.bytes);
    }
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    (decode_output(&bytes), truncated)
}

/// Decodes captured console output the way reference `decode_safe` does for a
/// subprocess: a byte-order mark decides the codec, and anything unmarked is
/// read as UTF-8 with replacement.
///
/// PowerShell emits UTF-16 often enough that this is the difference between
/// text and a string of interleaved NULs, so a stream that carries no mark but
/// is plainly UTF-16 is decoded as such too. That is what charset detection
/// buys the reference, narrowed here to the one encoding a Windows shell
/// actually produces.
pub(super) fn decode_output(bytes: &[u8]) -> String {
    if let Some(body) = bytes.strip_prefix(b"\xef\xbb\xbf") {
        return String::from_utf8_lossy(body).into_owned();
    }
    if let Some(body) = bytes.strip_prefix(b"\xff\xfe") {
        return decode_utf16(body, true);
    }
    if let Some(body) = bytes.strip_prefix(b"\xfe\xff") {
        return decode_utf16(body, false);
    }
    match utf16_endianness(bytes) {
        Some(little_endian) => decode_utf16(bytes, little_endian),
        None => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> String {
    let units = bytes.chunks_exact(2).map(|pair| {
        let (low, high) = (pair.first().copied(), pair.get(1).copied());
        let pair = [low.unwrap_or(0), high.unwrap_or(0)];
        if little_endian {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    });
    char::decode_utf16(units)
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Whether an unmarked stream is UTF-16, and in which order.
///
/// Text encoded in it leaves a NUL in every other byte, a shape UTF-8 output
/// never has. Requiring the other parity to carry no NUL at all keeps binary
/// output, which has them everywhere, from being read as text.
fn utf16_endianness(bytes: &[u8]) -> Option<bool> {
    let sampled = bytes.len().min(512) & !1;
    if sampled < 4 {
        return None;
    }
    let (mut trailing, mut leading) = (0_usize, 0_usize);
    for (index, byte) in bytes.iter().take(sampled).enumerate() {
        if *byte == 0 {
            if index.is_multiple_of(2) {
                leading += 1;
            } else {
                trailing += 1;
            }
        }
    }
    let expected = sampled / 2;
    let threshold = expected - expected / 4;
    if trailing >= threshold && leading == 0 {
        return Some(true);
    }
    (leading >= threshold && trailing == 0).then_some(false)
}

/// Reads at most `limit` bytes of `path` starting at `cursor`.
///
/// A window bounded by bytes can end mid-character. Reference `_read_file_chunk`
/// drops the dangling lead so the next read picks it up whole, whenever more
/// bytes are coming: either because the file is longer than the window, or
/// because the session is still writing to it. Without that, a poll of a live
/// log emits a replacement character the next poll cannot take back.
pub(super) fn read_file_window(
    path: &Path,
    cursor: u64,
    limit: usize,
    running: bool,
) -> Result<(String, u64, bool), ToolError> {
    let mut file = std::fs::File::open(path).map_err(|error| {
        ToolError::Execution(format!("`{}` cannot be read: {error}", path.display()))
    })?;
    let size = file
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    if cursor >= size {
        return Ok((String::new(), size, false));
    }
    file.seek(SeekFrom::Start(cursor)).map_err(|error| {
        ToolError::Execution(format!("`{}` cannot be read: {error}", path.display()))
    })?;
    let mut buffer = vec![0_u8; limit];
    let read = file.read(&mut buffer).map_err(|error| {
        ToolError::Execution(format!("`{}` cannot be read: {error}", path.display()))
    })?;
    buffer.truncate(read);
    if running || size > cursor.saturating_add(read as u64) {
        trim_incomplete_utf8_suffix(&mut buffer);
    }
    let next_cursor = cursor.saturating_add(buffer.len() as u64);
    Ok((decode_output(&buffer), next_cursor, size > next_cursor))
}

/// How many bytes the UTF-8 sequence led by `lead` occupies, if it leads one.
fn utf8_sequence_length(lead: u8) -> Option<usize> {
    match lead {
        0x00..=0x7f => Some(1),
        0xc0..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf7 => Some(4),
        _ => None,
    }
}

/// Drops a trailing sequence the window cut short, reference
/// `_trim_incomplete_utf8_suffix`.
fn trim_incomplete_utf8_suffix(buffer: &mut Vec<u8>) {
    for back in 1..=buffer.len().min(4) {
        let byte = buffer[buffer.len() - back];
        if (0x80..0xc0).contains(&byte) {
            continue;
        }
        if utf8_sequence_length(byte).is_some_and(|expected| back < expected) {
            buffer.truncate(buffer.len() - back);
        }
        return;
    }
}

/// Moves `cursor` forward off the continuation bytes of a character that starts
/// before it, reference `_skip_utf8_continuation_prefix`.
///
/// A tail window is positioned by subtracting a byte budget from the file size,
/// so unlike a cursor the model supplies it can land inside a character. Reading
/// from there would decode the tail of one character as a replacement.
pub(super) fn skip_utf8_continuation_prefix(path: &Path, cursor: u64) -> u64 {
    if cursor == 0 {
        return cursor;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return cursor;
    };
    if file.seek(SeekFrom::Start(cursor)).is_err() {
        return cursor;
    }
    let mut prefix = [0_u8; 3];
    let Ok(read) = file.read(&mut prefix) else {
        return cursor;
    };
    for (index, byte) in prefix[..read].iter().enumerate() {
        if (0x80..0xc0).contains(byte) {
            continue;
        }
        return cursor.saturating_add(index as u64);
    }
    cursor.saturating_add(read as u64)
}
